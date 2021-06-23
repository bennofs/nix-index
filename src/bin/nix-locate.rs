//! Tool for searching for files in nixpkgs packages
#[macro_use]
extern crate clap;
extern crate grep;
extern crate nix_index;
extern crate separator;
extern crate xdg;
extern crate regex;
extern crate isatty;
extern crate ansi_term;

#[macro_use]
extern crate stderr;
extern crate thiserror;

use std::path::PathBuf;
use std::result;
use std::process;
use std::str;
use std::collections::HashSet;
use separator::Separatable;
use clap::{Arg, App, ArgMatches};
use regex::bytes::Regex;
use ansi_term::Colour::Red;

use nix_index::database;
use nix_index::files::{self, FileType, FileTreeEntry};
use thiserror::Error as ThisError;

#[derive(ThisError, Debug)]
pub enum Error {
    #[error("reading from the database at '{}' failed.\n\
    This may be caused by a corrupt or missing database, try (re)running `nix-index` to generate the database. \n\
    If the error persists please file a bug report at https://github.com/bennofs/nix-index.", .0.to_string_lossy())]
    ReadDatabase(PathBuf),
    #[error("constructing the regular expression from the pattern '{}' failed.", .0)]
    Grep(String)
}

/// The struct holding the parsed arguments for searching
struct Args {
    /// Path of the nix-index database.
    database: PathBuf,
    /// The pattern to search for. This is always in regex syntax.
    pattern: String,
    group: bool,
    hash: Option<String>,
    package_pattern: Option<String>,
    file_type: Vec<FileType>,
    only_toplevel: bool,
    color: bool,
    minimal: bool,
}

/// The main function of this module: searches with the given options in the database.
fn locate(args: &Args) -> Result<(), Error> {
    // Build the regular expression matcher
    let pattern = Regex::new(&args.pattern).map_err(|e| {
        Error::Grep(args.pattern.clone())
    })?;
    let package_pattern = if let Some(ref pat) = args.package_pattern {
        Some(Regex::new(pat).map_err(|e| Error::Grep(pat.clone()))?)
    } else {
        None
    };

    // Open the database
    let index_file = args.database.join("files");
    let db = database::Reader::open(&index_file).map_err(|e| {
        Error::ReadDatabase(index_file.clone())
    })?;

    let results = db.query(&pattern)
        .package_pattern(package_pattern.as_ref())
        .hash(args.hash.clone())
        .run()
        .map_err(|e| Error::Grep(args.pattern.clone()))?
        .filter(|v| {
            v.as_ref().ok().map_or(true, |v| {
                let &(ref store_path, FileTreeEntry { ref path, ref node }) = v;
                let m = pattern.find_iter(path).last().expect(
                    "path should match the pattern",
                );

                let conditions = [
                    !args.group || !path[m.end()..].contains(&b'/'),
                    !args.only_toplevel || (*store_path.origin()).toplevel,
                    args.file_type.iter().any(|t| &node.get_type() == t),
                ];

                conditions.iter().all(|c| *c)
            })
        });

    let mut printed_attrs = HashSet::new();
    for v in results {
        let (store_path, FileTreeEntry { path, node }) =
            v.map_err(|e| Error::ReadDatabase(index_file.clone()))?;

        use files::FileNode::*;
        let (typ, size) = match node {
            Regular { executable, size } => (if executable { "x" } else { "r" }, size),
            Directory { size, contents: () }=> ("d", size),
            Symlink { .. } => ("s", 0),
        };

        let mut attr = format!(
            "{}.{}",
            store_path.origin().attr,
            store_path.origin().output
        );

        if !store_path.origin().toplevel {
            attr = format!("({})", attr);
        }

        if args.minimal {
            // only print each package once, even if there are multiple matches
            if printed_attrs.insert(attr.clone()) {
                println!("{}", attr);
            }
        } else {
            print!(
                "{:<40} {:>14} {:>1} {}",
                attr,
                size.separated_string(),
                typ,
                store_path.as_str()
            );

            let path = String::from_utf8_lossy(&path);

            if args.color {
                let mut prev = 0;
                for mat in pattern.find_iter(path.as_bytes()) {
                    // if the match is empty, we need to make sure we don't use string
                    // indexing because the match may be "inside" a single multibyte character
                    // in that case (for example, the pattern may match the second byte of a multibyte character)
                    if mat.start() == mat.end() {
                        continue;
                    }
                    print!(
                        "{}{}",
                        &path[prev..mat.start()],
                        Red.paint(&path[mat.start()..mat.end()])
                    );
                    prev = mat.end();
                }
                println!("{}", &path[prev..]);
            } else {
                println!("{}", path);
            }
        }
    }

    Ok(())
}

/// Extract the parsed arguments for clap's arg matches.
///
/// Handles parsing the values of more complex arguments.
fn process_args(matches: &ArgMatches) -> result::Result<Args, clap::Error> {
    let pattern_arg = matches
        .value_of("PATTERN")
        .expect("pattern arg required")
        .to_string();
    let package_arg = matches.value_of("package");
    let start_anchor = if matches.is_present("at-root") {
        "^"
    } else {
        ""
    };
    let end_anchor = if matches.is_present("whole") { "$" } else { "" };
    let make_pattern = |s: &str, wrap: bool| {
        let regex = if matches.is_present("regex") {
            s.to_string()
        } else {
            regex::escape(s)
        };
        if wrap {
            format!("{}{}{}", start_anchor, regex, end_anchor)
        } else {
            regex
        }
    };
    let color = matches.value_of("color").and_then(|x| {
        if x == "auto" {
            return None;
        }
        if x == "always" {
            return Some(true);
        }
        if x == "never" {
            return Some(false);
        }
        unreachable!("color can only be auto, always or never (verified by clap already)")
    });
    let args = Args {
        database: PathBuf::from(matches.value_of("database").expect("database has default value by clap")),
        group: !matches.is_present("no-group"),
        pattern: make_pattern(&pattern_arg, true),
        package_pattern: package_arg.map(|p| make_pattern(p, false)),
        hash: matches.value_of("hash").map(str::to_string),
        file_type: matches.values_of("type").map_or(files::ALL_FILE_TYPES.to_vec(), |types| {
            types.map(|t| match t {
                "x" => FileType::Regular { executable: true },
                "r" => FileType::Regular { executable: false },
                "s" => FileType::Symlink,
                "d" => FileType::Directory,
                _ => unreachable!("file type can only be one of x, r, s and d (verified by clap already)"),
            }).collect()
        }),
        only_toplevel: matches.is_present("toplevel"),
        color: color.unwrap_or_else(isatty::stdout_isatty),
        minimal: matches.is_present("minimal"),
    };
    Ok(args)
}

const LONG_USAGE: &'static str = r#"
How to use
==========

In the simplest case, just run `nix-locate part/of/file/path` to search for all packages that contain
a file matching that path:

$ nix-locate 'bin/firefox'
...all packages containing a file named 'bin/firefox'

Before using this tool, you first need to generate a nix-index database.
Use the `nix-index` tool to do that.

Limitations
===========

* this tool can only find packages which are built by hydra, because only those packages
  will have file listings that are indexed by nix-index

* we can't know the precise attribute path for every package, so if you see the syntax `(attr)`
  in the output, that means that `attr` is not the target package but that it
  depends (perhaps indirectly) on the package that contains the searched file. Example:

  $ nix-locate 'bin/xmonad'
  (xmonad-with-packages.out)      0 s /nix/store/nl581g5kv3m2xnmmfgb678n91d7ll4vv-ghc-8.0.2-with-packages/bin/xmonad

  This means that we don't know what nixpkgs attribute produces /nix/store/nl581g5kv3m2xnmmfgb678n91d7ll4vv-ghc-8.0.2-with-packages,
  but we know that `xmonad-with-packages.out` requires it.
"#;

fn main() {
    let base = xdg::BaseDirectories::with_prefix("nix-index").unwrap();
    let cache_dir = base.get_cache_home();
    let cache_dir = cache_dir.to_string_lossy();

    let matches = App::new("Nixpkgs Files Indexer")
        .version(crate_version!())
        .author(crate_authors!())
        .about("Quickly finds the derivation providing a certain file")
        .arg(Arg::with_name("database")
             .short("d")
             .long("db")
             .default_value(&cache_dir)
             .help("Directory where the index is stored"))
        .arg(Arg::with_name("PATTERN")
             .required(true)
             .help("Pattern for which to search")
             .index(1))
        .arg(Arg::with_name("regex")
             .short("r")
             .long("regex")
             .help("Treat PATTERN as regex instead of literal text. Also applies to --name option."))
        .arg(Arg::with_name("package")
             .short("p")
             .long("package")
             .value_name("PATTERN")
             .help("Only print matches from packages whose name matches PATTERN."))
        .arg(Arg::with_name("hash")
             .long("hash")
             .value_name("HASH")
             .help("Only print matches from the package that has the given HASH."))
        .arg(Arg::with_name("toplevel")
             .long("top-level")
             .help("Only print matches from packages that show up in nix-env -qa."))
        .arg(Arg::with_name("type")
             .short("t")
             .long("type")
             .multiple(true)
             .number_of_values(1)
             .value_name("TYPE")
             .possible_values(&["d", "x", "r", "s"])
             .help("Only print matches for files that have this type.\
                    If the option is given multiple times, a file will be printed if it has any of the given types."
             ))
         .arg(Arg::with_name("no-group")
              .long("no-group")
              .help("Disables grouping of paths with the same matching part. \n\
                     By default, a path will only be printed if the pattern matches some part\n\
                     of the last component of the path. For example, the pattern `a/foo` would\n\
                     match all of `a/foo`, `a/foo/some_file` and `a/foo/another_file`, but only\n\
                     the first match will be printed. This option disables that behavior and prints\n\
                     all matches."
              ))
        .arg(Arg::with_name("color")
             .multiple(false)
             .value_name("COLOR")
             .possible_values(&["always", "never", "auto"])
             .help("Whether to use colors in output. If auto, only use colors if outputting to a terminal."))
        .arg(Arg::with_name("whole")
             .short("w")
             .long("whole-name")
             .help("Only print matches for files or directories whose basename matches PATTERN exactly.\n\
                    This means that the pattern `bin/foo` will only match a file called `bin/foo` or `xx/bin/foo`\n\
                    but not `bin/foobar`."
             ))
        .arg(Arg::with_name("at-root")
            .long("at-root")
            .help("Treat PATTERN as an absolute file path, so it only matches starting from the root of a package.\n\
                   This means that the pattern `/bin/foo` only matches a file called `/bin/foo` or `/bin/foobar`\n\
                   but not `/libexec/bin/foo`."
            ))
        .arg(Arg::with_name("minimal")
             .short("1")
             .long("minimal")
             .help("Only print attribute names of found files or directories.\n\
                    Other details such as size or store path are omitted.\n\
                    This is useful for scripts that use the output of nix-locate."
             ))
        .after_help(LONG_USAGE)
        .get_matches();


    let args = process_args(&matches).unwrap_or_else(|e| e.exit());

    if let Err(e) = locate(&args) {
        errln!("error: {}", e);

        // for e in e.iter().skip(1) {
        //     errln!("caused by: {}", e);
        // }

        // if let Some(backtrace) = e.backtrace() {
        //     errln!("backtrace: {:?}", backtrace);
        // }
        process::exit(2);
    }
}
