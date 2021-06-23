//! Small but reusable helper functions.
use std::io::{self, Write};
use std::path::PathBuf;
use std::fs::OpenOptions;
use std::env;

use futures::IntoFuture;
use futures::future::{self, Either, FutureResult};

/// Writes a file to the temp directory with a name that is made of the supplied
/// base and a suffix if a file with that name already exists.
///
/// Returns the path of the file if the file was written successfully, None otherwise.
/// None means that an IO error occurred during writing the file.
pub fn write_temp_file(base_name: &str, contents: &[u8]) -> Option<PathBuf> {
    let mut path = None;
    for i in 0.. {
        let mut this_path = env::temp_dir();
        if i == 0 {
            this_path.push(base_name);
        } else {
            this_path.push(format!("{}.{}", base_name, i));
        }
        let temp_file = OpenOptions::new().write(true).create_new(true).open(
            &this_path,
        );
        match temp_file {
            Ok(mut file) => {
                path = file.write_all(contents).map(|_| this_path).ok();
                break;
            }
            Err(e) => {
                if e.kind() != io::ErrorKind::AlreadyExists {
                    break;
                }
            }
        }
    }
    path
}
