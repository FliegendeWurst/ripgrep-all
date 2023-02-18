use crate::adapted_iter::AdaptedFilesIterBox;
use crate::adapters::*;
use crate::caching_writer::async_read_and_write_to_cache;
use crate::config::RgaConfig;
use crate::matching::*;
use crate::recurse::concat_read_streams;
use crate::{
    preproc_cache::{LmdbCache, PreprocCache},
    print_bytes,
};
use anyhow::*;
use async_compression::tokio::bufread::ZstdDecoder;
use async_stream::stream;
use log::*;
use path_clean::PathClean;
use postproc::PostprocPrefix;
use std::io::Cursor;
use std::path::Path;
use std::sync::Arc;
use tokio::io::AsyncBufRead;
use tokio::io::AsyncBufReadExt;
use tokio::io::BufReader;

type ActiveAdapters = Vec<Arc<dyn FileAdapter>>;

async fn choose_adapter(
    config: &RgaConfig,
    filepath_hint: &Path,
    archive_recursion_depth: i32,
    inp: &mut (impl AsyncBufRead + Unpin),
) -> Result<Option<(Arc<dyn FileAdapter>, FileMatcher, ActiveAdapters)>> {
    let active_adapters = get_adapters_filtered(config.custom_adapters.clone(), &config.adapters)?;
    let adapters = adapter_matcher(&active_adapters, config.accurate)?;
    let filename = filepath_hint
        .file_name()
        .ok_or_else(|| format_err!("Empty filename"))?;
    debug!("Archive recursion depth: {}", archive_recursion_depth);

    let mimetype = if config.accurate {
        let buf = inp.fill_buf().await?; // fill but do not consume!
        let mimetype = tree_magic::from_u8(buf);
        debug!("mimetype: {:?}", mimetype);
        Some(mimetype)
    } else {
        None
    };
    let adapter = adapters(FileMeta {
        mimetype,
        lossy_filename: filename.to_string_lossy().to_string(),
    });
    Ok(adapter.map(|e| (e.0, e.1, active_adapters)))
}

enum Ret {
    Recurse(AdaptInfo, Arc<dyn FileAdapter>, FileMatcher, ActiveAdapters),
    Passthrough(AdaptInfo),
}
async fn buf_choose_adapter(ai: AdaptInfo) -> Result<Ret> {
    let mut inp = BufReader::with_capacity(1 << 16, ai.inp);
    let adapter = choose_adapter(
        &ai.config,
        &ai.filepath_hint,
        ai.archive_recursion_depth,
        &mut inp,
    )
    .await?;
    let ai = AdaptInfo {
        inp: Box::pin(inp),
        ..ai
    };
    let (a, b, c) = match adapter {
        Some(x) => x,
        None => {
            // allow passthrough if the file is in an archive or accurate matching is enabled
            // otherwise it should have been filtered out by rg pre-glob since rg can handle those better than us
            let allow_cat = !ai.is_real_file || ai.config.accurate;
            if allow_cat {
                if ai.postprocess {
                    (
                        Arc::new(PostprocPrefix {}) as Arc<dyn FileAdapter>,
                        FileMatcher::Fast(FastFileMatcher::FileExtension("default".to_string())),
                        Vec::new(),
                    )
                } else {
                    return Ok(Ret::Passthrough(ai));
                }
            } else {
                return Err(format_err!(
                    "No adapter found for file {:?}, passthrough disabled.",
                    ai.filepath_hint
                        .file_name()
                        .ok_or_else(|| format_err!("Empty filename"))?
                ));
            }
        }
    };
    Ok(Ret::Recurse(ai, a, b, c))
}

/**
 * preprocess a file as defined in `ai`.
 *
 * If a cache is passed, read/write to it.
 *
 */
pub async fn rga_preproc(ai: AdaptInfo) -> Result<ReadBox> {
    debug!("path (hint) to preprocess: {:?}", ai.filepath_hint);

    // todo: figure out when using a bufreader is a good idea and when it is not
    // seems to be good for File::open() reads, but not sure about within archives (tar, zip)
    let (ai, adapter, detection_reason, active_adapters) = match buf_choose_adapter(ai).await? {
        Ret::Recurse(ai, a, b, c) => (ai, a, b, c),
        Ret::Passthrough(ai) => {
            return Ok(ai.inp);
        }
    };
    let path_hint_copy = ai.filepath_hint.clone();
    adapt_caching(ai, adapter, detection_reason, active_adapters)
        .await
        .with_context(|| format!("run_adapter({})", &path_hint_copy.to_string_lossy()))
}

fn compute_cache_key(
    filepath_hint: &Path,
    adapter: &dyn FileAdapter,
    active_adapters: ActiveAdapters,
) -> Result<Vec<u8>> {
    let clean_path = filepath_hint.to_owned().clean();
    let meta = std::fs::metadata(filepath_hint)
        .with_context(|| format!("reading metadata for {}", filepath_hint.to_string_lossy()))?;
    let modified = meta.modified().expect("weird OS that can't into mtime");

    if adapter.metadata().recurses {
        let active_adapters_cache_key = active_adapters
            .iter()
            .map(|a| (a.metadata().name.clone(), a.metadata().version))
            .collect::<Vec<_>>();
        let key = (active_adapters_cache_key, clean_path, modified);
        debug!("Cache key (with recursion): {:?}", key);
        bincode::serialize(&key).context("could not serialize path")
    } else {
        let key = (
            adapter.metadata().name.clone(),
            adapter.metadata().version,
            clean_path,
            modified,
        );
        debug!("Cache key (no recursion): {:?}", key);
        bincode::serialize(&key).context("could not serialize path")
    }
}

async fn adapt_caching(
    ai: AdaptInfo,
    adapter: Arc<dyn FileAdapter>,
    detection_reason: FileMatcher,
    active_adapters: ActiveAdapters,
) -> Result<ReadBox> {
    let meta = adapter.metadata();
    debug!(
        "Chose adapter '{}' because of matcher {:?}",
        &meta.name, &detection_reason
    );
    eprintln!(
        "{} adapter: {}",
        ai.filepath_hint.to_string_lossy(),
        &meta.name
    );
    let db_name = format!("{}.v{}", meta.name, meta.version);
    let cache_compression_level = ai.config.cache.compression_level;
    let cache_max_blob_len = ai.config.cache.max_blob_len;

    let cache = if ai.is_real_file {
        LmdbCache::open(&ai.config.cache)?
    } else {
        None
    };

    let mut cache = cache.context("No cache?")?;
    let cache_key: Vec<u8> =
        compute_cache_key(&ai.filepath_hint, adapter.as_ref(), active_adapters)?;
    // let dbg_ctx = format!("adapter {}", &adapter.metadata().name);
    let cached = cache.get(&db_name, &cache_key)?;
    match cached {
        Some(cached) => Ok(Box::pin(ZstdDecoder::new(Cursor::new(cached)))),
        None => {
            debug!("cache MISS, running adapter with caching...");
            let inp = loop_adapt(adapter.as_ref(), detection_reason, ai)?;
            let inp = concat_read_streams(inp);
            let inp = async_read_and_write_to_cache(
                inp,
                cache_max_blob_len.0,
                cache_compression_level.0,
                Box::new(move |(uncompressed_size, compressed)| {
                    debug!(
                        "uncompressed output: {}",
                        print_bytes(uncompressed_size as f64)
                    );
                    if let Some(cached) = compressed {
                        debug!("compressed output: {}", print_bytes(cached.len() as f64));
                        cache.set(&db_name, &cache_key, &cached)?
                    }
                    Ok(())
                }),
            )?;

            Ok(Box::pin(inp))
        }
    }
}

pub fn loop_adapt(
    adapter: &dyn FileAdapter,
    detection_reason: FileMatcher,
    ai: AdaptInfo,
) -> anyhow::Result<AdaptedFilesIterBox> {
    let fph = ai.filepath_hint.clone();
    let inp = adapter.adapt(ai, &detection_reason).with_context(|| {
        format!(
            "adapting {} via {} failed",
            fph.to_string_lossy(),
            adapter.metadata().name
        )
    })?;
    let s = stream! {
        for await file in inp {
            match buf_choose_adapter(file?).await? {
                Ret::Recurse(ai, adapter, detection_reason, _active_adapters) => {
                    if ai.archive_recursion_depth >= ai.config.max_archive_recursion.0 {
                        let s = format!("{}[rga: max archive recursion reached ({})]", ai.line_prefix, ai.archive_recursion_depth).into_bytes();
                        yield Ok(AdaptInfo {
                            inp: Box::pin(Cursor::new(s)),
                            ..ai
                        });
                        continue;
                    }
                    debug!(
                        "Chose adapter '{}' because of matcher {:?}",
                        &adapter.metadata().name, &detection_reason
                    );
                    eprintln!(
                        "{} adapter: {}",
                        ai.filepath_hint.to_string_lossy(),
                        &adapter.metadata().name
                    );
                    for await ifile in loop_adapt(adapter.as_ref(), detection_reason, ai)? {
                        yield ifile;
                    }
                }
                Ret::Passthrough(ai) => {
                    debug!("no adapter for {}, ending recursion", ai.filepath_hint.to_string_lossy());
                    yield Ok(ai);
                }
            }
        }
    };
    Ok(Box::pin(s))
}
