use super::*;
use crate::{adapted_iter::AdaptedFilesIter, print_bytes};
use anyhow::*;
use async_stream::stream;
use async_zip::read::stream::ZipFileReader;
use lazy_static::lazy_static;
use log::*;

static EXTENSIONS: &[&str] = &["zip"];

lazy_static! {
    static ref METADATA: AdapterMeta = AdapterMeta {
        name: "zip".to_owned(),
        version: 1,
        description: "Reads a zip file as a stream and recurses down into its contents".to_owned(),
        recurses: true,
        fast_matchers: EXTENSIONS
            .iter()
            .map(|s| FastFileMatcher::FileExtension(s.to_string()))
            .collect(),
        slow_matchers: Some(vec![FileMatcher::MimeType("application/zip".to_owned())]),
        keep_fast_matchers_if_accurate: false,
        disabled_by_default: false
    };
}
#[derive(Default, Clone)]
pub struct ZipAdapter;

impl ZipAdapter {
    pub fn new() -> ZipAdapter {
        ZipAdapter
    }
}
impl GetMetadata for ZipAdapter {
    fn metadata(&self) -> &AdapterMeta {
        &METADATA
    }
}

impl FileAdapter for ZipAdapter {
    fn adapt(&self, ai: AdaptInfo, _detection_reason: &FileMatcher) -> Result<AdaptedFilesIterBox> {
        let AdaptInfo {
            inp,
            filepath_hint,
            archive_recursion_depth,
            postprocess,
            line_prefix,
            config,
            ..
        } = ai;
        let mut zip = ZipFileReader::new(inp);

        let s = stream! {
            while !zip.finished() {
                if let Some(mut reader) = zip.entry_reader().await? {
                    let file = reader.entry();
                    /* if file.is_dir() {
                    continue;
                    }*/
                    debug!(
                        "{}{}|{}: {} ({} packed)",
                        line_prefix,
                        filepath_hint.display(),
                        file.filename(),
                        print_bytes(file.uncompressed_size() as f64),
                        print_bytes(file.compressed_size() as f64)
                    );
                    let new_line_prefix = format!("{}{}: ", line_prefix, file.filename());
                    yield Ok(AdaptInfo {
                        filepath_hint: PathBuf::from(file.filename()),
                        is_real_file: false,
                        inp: Box::pin(reader),
                        line_prefix: new_line_prefix,
                        archive_recursion_depth: archive_recursion_depth + 1,
                        postprocess,
                        config: config.clone(),
                    });
                }
            }
        };
        Ok(Box::pin(s))
    }
}

/*struct ZipAdaptIter {
    inp: AdaptInfo,
}
impl<'a> AdaptedFilesIter for ZipAdaptIter<'a> {
    fn next<'b>(&'b mut self) -> Option<AdaptInfo<'b>> {
        let line_prefix = &self.inp.line_prefix;
        let filepath_hint = &self.inp.filepath_hint;
        let archive_recursion_depth = &self.inp.archive_recursion_depth;
        let postprocess = self.inp.postprocess;
        ::zip::read::read_zipfile_from_stream(&mut self.inp.inp)
            .unwrap()
            .and_then(|file| {
                if file.is_dir() {
                    return None;
                }
                debug!(
                    "{}{}|{}: {} ({} packed)",
                    line_prefix,
                    filepath_hint.to_string_lossy(),
                    file.name(),
                    print_bytes(file.size() as f64),
                    print_bytes(file.compressed_size() as f64)
                );
                let line_prefix = format!("{}{}: ", line_prefix, file.name());
                Some(AdaptInfo {
                    filepath_hint: PathBuf::from(file.name()),
                    is_real_file: false,
                    inp: Box::new(file),
                    line_prefix,
                    archive_recursion_depth: archive_recursion_depth + 1,
                    postprocess,
                    config: RgaConfig::default(), //config.clone(),
                })
            })
    }
}*/

#[cfg(test)]
mod test {
    use super::*;
    use crate::test_utils::*;

    fn create_zip(fname: &str, content: &str, add_inner: bool) -> Result<Vec<u8>> {
        use ::zip::write::FileOptions;
        use std::io::Write;

        // We use a buffer here, though you'd normally use a `File`
        let mut zip = ::zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));

        let options = FileOptions::default().compression_method(::zip::CompressionMethod::Stored);
        zip.start_file(fname, options)?;
        zip.write(content.as_bytes())?;

        if add_inner {
            zip.start_file("inner.zip", options)?;
            zip.write(&create_zip("inner.txt", "inner text file", false)?)?;
        }
        // Apply the changes you've made.
        // Dropping the `ZipWriter` will have the same effect, but may silently fail
        Ok(zip.finish()?.into_inner())
    }
    #[test]
    fn recurse() -> Result<()> {
        let zipfile = create_zip("outer.txt", "outer text file", true)?;
        let adapter: Box<dyn FileAdapter> = Box::new(ZipAdapter::new());

        let (a, d) = simple_adapt_info(
            &PathBuf::from("outer.zip"),
            Box::new(std::io::Cursor::new(zipfile)),
        );
        let buf = adapted_to_vec(adapter.adapt(a, &d)?)?;

        assert_eq!(
            String::from_utf8(buf)?,
            "PREFIX:outer.txt: outer text file\nPREFIX:inner.zip: inner.txt: inner text file\n",
        );

        Ok(())
    }
}
