//! Read package information from nix-env.
//!
//! This module implements the gathering of initial set of root store paths to fetch.
//! We parse the output `nix-env --query` to figure out all accessible store paths with their attribute path
//! and hashes.
use xml;
use std::io::{self, Read};
use xml::reader::{EventReader, XmlEvent};
use xml::common::{TextPosition, Position};
use std::process::{Command, Stdio, Child, ChildStdout};
use std::fmt;
use std::error;

use crate::package::{PathOrigin, StorePath};

/// Calls `nix-env` to list the packages in the given nixpkgs.
///
/// The `nixpkgs` argument can either be a path to a nixpkgs checkout or another expression
/// accepted by `nix-env -f`, such as `<nixpkgs>` or `http://example.org/nixpkgs.tar.bz`.
///
/// If scope is `Some(attr)`, nix-env is called with the `-A attr` argument so only packages that are a member
/// of `attr` are returned.
///
/// The function returns an Iterator over the packages returned by nix-env.
pub fn query_packages(
    nixpkgs: &str,
    scope: Option<&str>,
    show_trace: bool,
) -> PackagesQuery<ChildStdout> {
    let mut cmd = Command::new("nix-env");
    cmd.arg("-qaP")
        .arg("--out-path")
        .arg("--xml")
        .arg("--arg")
        .arg("config")
        .arg("{}")
        .arg("--file")
        .arg(nixpkgs)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null());

    if let Some(scope) = scope {
        cmd.arg("-A").arg(scope);
    }

    if show_trace {
        cmd.arg("--show-trace");
    }

    PackagesQuery {
        parser: None,
        child: None,
        cmd: Some(cmd),
    }
}

/// An iterator that parses the output of nix-env and returns parsed store paths.
///
/// Use `query_packages` to create a value of this type.
pub struct PackagesQuery<R: Read> {
    parser: Option<PackagesParser<R>>,
    child: Option<Child>,
    cmd: Option<Command>,
}

impl PackagesQuery<ChildStdout> {
    /// Spawns the nix-env subprocess and initializes the parser.
    ///
    /// If the subprocess was already spawned, does nothing.
    fn ensure_initialized(&mut self) -> Result<(), Error> {
        if let Some(mut cmd) = self.cmd.take() {
            let mut child = cmd.spawn()?;

            let stdout = child.stdout.take().expect("should have stdout pipe");
            let parser = PackagesParser::new(stdout);

            self.child = Some(child);
            self.parser = Some(parser);
        }
        Ok(())
    }

    /// Waits for the subprocess to exit and checks whether it has returned a non-zero exit code
    /// (= failed with an error).
    ///
    /// If the exit code was non-zero, returns Some(err), else it returns None.
    fn check_error(&mut self) -> Option<Error> {
        let mut run = || {
            let child = match self.child.take() {
                Some(c) => c,
                None => return Ok(()),
            };
            let result = child.wait_with_output()?;

            if !result.status.success() {
                let message = String::from_utf8_lossy(&result.stderr);

                return Err(Error::Command(match result.status.code() {
                    Some(c) => format!("nix-env failed with exit code {}:\n{}", c, message),
                    None => format!("nix-env failed with unknown exit code:\n{}", message),
                }));
            }

            Ok(())
        };

        run().err()
    }
}

impl Iterator for PackagesQuery<ChildStdout> {
    type Item = Result<StorePath, Error>;
    fn next(&mut self) -> Option<Self::Item> {
        if let Err(e) = self.ensure_initialized() {
            return Some(Err(e));
        }
        self.parser
            .take()
            .and_then(|mut parser| {
                parser
                    .next()
                    .map(|v| {
                        self.parser = Some(parser);
                        // When the parser throws an error, we first wait for the subprocess to exit.
                        //
                        // If the subprocess returned an error, then the parser probably tried to parse garbage output
                        // so we will ignore the parser error and instead return the error printed by the subprocess.
                        v.map_err(|e| self.check_error().unwrap_or_else(|| Error::from(e)))
                    })
                    .or_else(|| {
                        self.parser = None;
                        // At the end, we should check if the subprocess exited successfully.
                        self.check_error().map(Err)
                    })
            })
    }
}

/// Parses the XML output of `nix-env` and returns individual store paths.
struct PackagesParser<R: Read> {
    events: EventReader<R>,
    current_item: Option<String>,
}

/// A parser error that may occur during parsing `nix-env`'s output.
#[derive(Debug)]
pub struct ParserError {
    position: TextPosition,
    kind: ParserErrorKind,
}

/// Enumerates all possible error kinds that may occur during parsing.
#[derive(Debug)]
pub enum ParserErrorKind {
    /// Found an element with the tag `element_name` that should only occur inside
    /// elements with the tag `expected_parent` but it occurred as child of a different parent.
    MissingParent {
        element_name: String,
        expected_parent: String,
    },

    /// An element occurred as a child of `found_parent`, but
    /// we know that elements with the tag `element_name` should never have that as
    /// a parent.
    ParentNotAllowed {
        element_name: String,
        found_parent: String,
    },

    /// The required attribute `attribute_name` was missing on an element with the tag `element_name`.
    MissingAttribute {
        element_name: String,
        attribute_name: String,
    },

    /// Found the end tag for `element_name` without a matching start tag.
    MissingStartTag { element_name: String },

    /// An XML syntax error.
    XmlError { error: xml::reader::Error },

    /// A store path in the output of `nix-env` could not be parsed. All valid store paths
    /// need to match the format `$(STOREDIR)$(HASH)-$(NAME)`.
    InvalidStorePath { path: String },
}

impl fmt::Display for ParserError {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        use self::ParserErrorKind::*;
        write!(f, "error at {}: ", self.position)?;
        match self.kind {
            MissingParent {
                ref element_name,
                ref expected_parent,
            } => {
                write!(
                    f,
                    "element {} appears outside of expected parent {}",
                    element_name,
                    expected_parent
                )
            }
            ParentNotAllowed {
                ref element_name,
                ref found_parent,
            } => {
                write!(
                    f,
                    "element {} must not appear as child of {}",
                    element_name,
                    found_parent
                )
            }
            MissingAttribute {
                ref element_name,
                ref attribute_name,
            } => {
                write!(
                    f,
                    "element {} must have an attribute named {}",
                    element_name,
                    attribute_name
                )
            }
            MissingStartTag { ref element_name } => {
                write!(f, "element {} does not have a start tag", element_name)
            }
            XmlError { ref error } => write!(f, "document not well-formed: {}", error),
            InvalidStorePath { ref path } => {
                write!(
                    f,
                    "store path does not match expected format /prefix/hash-name: {}",
                    path
                )
            }
        }
    }
}

impl<R: Read> PackagesParser<R> {
    /// Creates a new parser that reads the `nix-env` XML output from the given reader.
    pub fn new(reader: R) -> PackagesParser<R> {
        PackagesParser {
            events: EventReader::new(reader),
            current_item: None,
        }
    }

    /// Shorthand for exiting with an error at the current position.
    fn err(&self, kind: ParserErrorKind) -> ParserError {
        ParserError {
            position: self.events.position(),
            kind: kind,
        }
    }

    /// Tries to read the next `StorePath` from the reader or fail with an error
    /// if there was a parse failure.
    ///
    /// Returns Ok(None) if the end of the stream was reached.
    ///
    /// This function is like `.next` from `Iterator`, but allows us to use `try! / ?` since it
    /// returns `Result<Option<...>, ...>` instead of `Option<Result<..., ...>>`.
    fn next_err(&mut self) -> Result<Option<StorePath>, ParserError> {
        use self::XmlEvent::*;
        use self::ParserErrorKind::*;

        loop {
            let event = self.events.next().map_err(
                |e| self.err(XmlError { error: e }),
            )?;
            match event {
                StartElement {
                    name: element_name,
                    attributes,
                    ..
                } => {
                    if element_name.local_name == "item" {
                        if !self.current_item.is_none() {
                            return Err(self.err(ParentNotAllowed {
                                element_name: "item".to_string(),
                                found_parent: "item".to_string(),
                            }));
                        }

                        let attr_path = attributes.into_iter().find(
                            |a| a.name.local_name == "attrPath",
                        );
                        let attr_path = attr_path.ok_or_else(|| {
                            self.err(MissingAttribute {
                                element_name: "item".into(),
                                attribute_name: "attrPath".into(),
                            })
                        })?;

                        self.current_item = Some(attr_path.value);
                        continue;
                    }

                    if element_name.local_name == "output" {
                        if let Some(item) = self.current_item.clone() {
                            let mut output_name = None;
                            let mut output_path = None;

                            for attr in attributes {
                                if attr.name.local_name == "name" {
                                    output_name = Some(attr.value);
                                    continue;
                                }

                                if attr.name.local_name == "path" {
                                    output_path = Some(attr.value);
                                    continue;
                                }
                            }

                            let output_name = output_name.ok_or_else(|| {
                                self.err(MissingAttribute {
                                    element_name: "output".into(),
                                    attribute_name: "name".into(),
                                })
                            })?;

                            let output_path = output_path.ok_or_else(|| {
                                self.err(MissingAttribute {
                                    element_name: "output".into(),
                                    attribute_name: "path".into(),
                                })
                            })?;

                            let origin = PathOrigin {
                                attr: item,
                                output: output_name,
                                toplevel: true,
                            };
                            let store_path = StorePath::parse(origin, &output_path);
                            let store_path = store_path.ok_or_else(|| {
                                self.err(InvalidStorePath { path: output_path })
                            })?;

                            return Ok(Some(store_path));
                        } else {
                            return Err(self.err(MissingParent {
                                element_name: "output".into(),
                                expected_parent: "item".into(),
                            }));
                        }
                    }
                }

                EndElement { name: element_name } => {
                    if element_name.local_name == "item" {
                        if self.current_item.is_none() {
                            return Err(self.err(MissingStartTag { element_name: "item".into() }));
                        }
                        self.current_item = None
                    }
                }

                EndDocument => break,

                _ => {}
            }
        }

        Ok(None)
    }
}

impl<R: Read> Iterator for PackagesParser<R> {
    type Item = Result<StorePath, ParserError>;

    fn next(&mut self) -> Option<Result<StorePath, ParserError>> {
        match self.next_err() {
            Err(e) => Some(Err(e)),
            Ok(Some(i)) => Some(Ok(i)),
            Ok(None) => None,
        }

    }
}

/// Enumeration of all the possible errors that may happen during querying the packages.
#[derive(Debug)]
pub enum Error {
    /// Parsing of the output failed
    Parse(ParserError),

    /// An IO error occurred
    Io(io::Error),

    /// nix-env failed with an error message
    Command(String),
}

impl error::Error for Error {
    fn description(&self) -> &str {
        match *self {
            Error::Parse(_) => "nix-env output parse error",
            Error::Io(_) => "io error",
            Error::Command(_) => "nix-env error",
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        use self::Error::*;
        match *self {
            Parse(ref e) => write!(f, "parsing XML output of nix-env failed: {}", e),
            Io(ref e) => write!(f, "IO error: {}", e),
            Command(ref e) => write!(f, "nix-env failed with error: {}", e),
        }
    }
}

impl From<io::Error> for Error {
    fn from(err: io::Error) -> Error {
        Error::Io(err)
    }
}


impl From<ParserError> for Error {
    fn from(err: ParserError) -> Error {
        Error::Parse(err)
    }
}
