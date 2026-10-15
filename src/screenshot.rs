//! Full application screenshots, including the shared title bar and version.
use slint::{Rgba8Pixel, SharedPixelBuffer};
use std::{
    fs::{File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

const SOFTWARE: &str = concat!("memtest ", env!("CARGO_PKG_VERSION"));

pub(super) fn save(window: &slint::Window) -> Result<Option<PathBuf>, String> {
    // Capture before the dialog or status changes can alter the displayed scene.
    let pixels = window
        .take_snapshot()
        .map_err(|e| format!("Cannot capture window: {e}"))?;
    let parent = window.window_handle();
    let Some(path) = rfd::FileDialog::new()
        .set_parent(&parent)
        .set_title("Save memtest screenshot")
        .set_file_name("memtest.png")
        .add_filter("PNG image", &["png"])
        .save_file()
    else {
        return Ok(None);
    };
    write_png(&path, &pixels).map(Some)
}

fn encode(writer: impl Write, pixels: &SharedPixelBuffer<Rgba8Pixel>) -> Result<(), String> {
    let mut encoder = png::Encoder::new(writer, pixels.width(), pixels.height());
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder
        .add_text_chunk("Software".into(), SOFTWARE.into())
        .map_err(|e| e.to_string())?;
    let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
    writer
        .write_image_data(pixels.as_bytes())
        .map_err(|e| e.to_string())?;
    writer.finish().map_err(|e| e.to_string())
}

fn png_path(path: &Path) -> PathBuf {
    if path
        .extension()
        .is_none_or(|extension| extension.is_empty())
    {
        path.with_extension("png")
    } else {
        path.to_owned()
    }
}

struct PendingFile {
    path: PathBuf,
    keep: bool,
}

impl Drop for PendingFile {
    fn drop(&mut self) {
        if !self.keep {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn write_output(
    path: &Path,
    create_new: bool,
    write: impl FnOnce(&mut File) -> io::Result<()>,
) -> io::Result<()> {
    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
    let (mut pending, file) = if create_new {
        let file = OpenOptions::new().write(true).create_new(true).open(path)?;
        (
            PendingFile {
                path: path.to_owned(),
                keep: false,
            },
            file,
        )
    } else {
        let directory = path.parent().unwrap_or(Path::new("."));
        loop {
            let temporary = directory.join(format!(
                ".memtest-{}-{}.tmp",
                std::process::id(),
                NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
            ));
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)
            {
                Ok(file) => {
                    break (
                        PendingFile {
                            path: temporary,
                            keep: false,
                        },
                        file,
                    );
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
    };
    // Close the file before replacing or cleaning it up, including on Windows.
    {
        let mut file = file;
        write(&mut file)?;
        file.sync_all()?;
    }
    if !create_new {
        std::fs::rename(&pending.path, path)?;
    }
    pending.keep = true;
    Ok(())
}

fn write_png(selected: &Path, pixels: &SharedPixelBuffer<Rgba8Pixel>) -> Result<PathBuf, String> {
    let path = png_path(selected);
    let mut encoded = Vec::new();
    encode(&mut encoded, pixels).map_err(|error| format!("Cannot encode screenshot: {error}"))?;
    let changed = path != selected;
    // The native dialog confirmed only `selected`. An appended extension must
    // not silently overwrite a different file, including one created meanwhile.
    write_output(&path, changed, |file| file.write_all(&encoded)).map_err(|error| {
        if changed && error.kind() == std::io::ErrorKind::AlreadyExists {
            format!(
                "'{}' already exists; select its full PNG filename in Save As to replace it",
                path.display()
            )
        } else {
            format!("Cannot save '{}': {error}", path.display())
        }
    })?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, io::Cursor};

    fn pixels() -> SharedPixelBuffer<Rgba8Pixel> {
        SharedPixelBuffer::clone_from_slice(
            &[255u8, 0, 0, 255, 0, 255, 0, 128, 0, 0, 255, 255, 0, 0, 0, 0],
            2,
            2,
        )
    }

    fn temporary(tag: &str) -> PathBuf {
        let time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "memtest-screenshot-{}-{time}-{tag}",
            std::process::id()
        ))
    }

    struct RemoveFile(PathBuf);
    impl Drop for RemoveFile {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }

    struct RemoveDirectory(PathBuf);
    impl Drop for RemoveDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir(&self.0);
        }
    }

    #[test]
    fn png_roundtrip_preserves_rgba_dimensions_and_software() {
        let pixels = pixels();
        let mut encoded = Vec::new();
        encode(&mut encoded, &pixels).unwrap();
        assert_eq!(&encoded[..8], b"\x89PNG\r\n\x1a\n");
        let mut reader = png::Decoder::new(Cursor::new(encoded)).read_info().unwrap();
        assert_eq!((reader.info().width, reader.info().height), (2, 2));
        assert!(
            reader
                .info()
                .uncompressed_latin1_text
                .iter()
                .any(|text| text.keyword == "Software" && text.text == SOFTWARE)
        );
        let mut decoded = vec![0; reader.output_buffer_size().unwrap()];
        let frame = reader.next_frame(&mut decoded).unwrap();
        assert_eq!(frame.color_type, png::ColorType::Rgba);
        assert_eq!(frame.bit_depth, png::BitDepth::Eight);
        assert_eq!(&decoded[..frame.buffer_size()], pixels.as_bytes());
    }

    #[test]
    fn missing_extension_is_added_and_collision_never_overwrites() {
        assert_eq!(png_path(Path::new("result")), Path::new("result.png"));
        assert_eq!(png_path(Path::new("result.")), Path::new("result.png"));
        assert_eq!(png_path(Path::new("result.PNG")), Path::new("result.PNG"));
        let selected = temporary("collision");
        let path = png_path(&selected);
        let _cleanup = RemoveFile(path.clone());
        let mut original = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        original.write_all(b"keep existing file").unwrap();
        drop(original);
        assert!(
            write_png(&selected, &pixels())
                .unwrap_err()
                .contains("already exists")
        );
        assert!(
            write_png(&path, &SharedPixelBuffer::new(0, 1))
                .unwrap_err()
                .contains("Cannot encode")
        );
        assert_eq!(fs::read(path).unwrap(), b"keep existing file");
    }

    #[test]
    fn writes_a_png_and_reports_file_and_encoder_failures() {
        let selected = temporary("saved");
        let expected = png_path(&selected);
        let _cleanup = RemoveFile(expected.clone());
        assert_eq!(write_png(&selected, &pixels()).unwrap(), expected);
        assert_eq!(&fs::read(&expected).unwrap()[..8], b"\x89PNG\r\n\x1a\n");
        fs::write(&expected, b"old confirmed image").unwrap();
        assert_eq!(write_png(&expected, &pixels()).unwrap(), expected);
        let mut reader = png::Decoder::new(Cursor::new(fs::read(&expected).unwrap()))
            .read_info()
            .unwrap();
        let mut decoded = vec![0; reader.output_buffer_size().unwrap()];
        let frame = reader.next_frame(&mut decoded).unwrap();
        assert_eq!(&decoded[..frame.buffer_size()], pixels().as_bytes());
        let absent_parent = temporary("absent").join("result.png");
        assert!(
            write_png(&absent_parent, &pixels())
                .unwrap_err()
                .contains("Cannot save")
        );

        struct FailWrite;
        impl Write for FailWrite {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("test write failure"))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        assert!(
            encode(FailWrite, &pixels())
                .unwrap_err()
                .contains("test write failure")
        );
    }

    #[test]
    fn failed_output_preserves_confirmed_image_and_cleans_partial_files() {
        let directory = temporary("failed-output");
        fs::create_dir(&directory).unwrap();
        let _directory_cleanup = RemoveDirectory(directory.clone());
        let existing = directory.join("confirmed.png");
        fs::write(&existing, b"original image").unwrap();
        let _file_cleanup = RemoveFile(existing.clone());
        let new = directory.join("new.png");
        for (path, create_new) in [(&existing, false), (&new, true)] {
            let failure = write_output(path, create_new, |file| {
                file.write_all(b"partial replacement")?;
                Err(io::Error::other("injected output failure"))
            });
            assert!(failure.unwrap_err().to_string().contains("injected"));
            assert_eq!(fs::read(&existing).unwrap(), b"original image");
            let remaining: Vec<_> = fs::read_dir(&directory)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .collect();
            assert_eq!(remaining, vec![existing.clone()]);
        }
        let blocked = directory.join("blocked.png");
        fs::create_dir(&blocked).unwrap();
        let _blocked_cleanup = RemoveDirectory(blocked.clone());
        assert!(write_output(&blocked, false, |file| file.write_all(b"replacement")).is_err());
        assert!(blocked.is_dir());
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 2);
    }
}
