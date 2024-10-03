//! Bindgen utilities for generating bindings to C/C++ code.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::{env, fs};

use anyhow::{anyhow, bail, Context, Error, Result};
use serde::Deserialize;

use crate::cargo::out_dir;
use crate::utils::OsStrExt;
use crate::{cargo, cmd};

/// The environment variable name containing the file path of the file that contains the
/// generated bindings.
pub const VAR_BINDINGS_FILE: &str = "EMBUILD_GENERATED_BINDINGS_FILE";

#[derive(Debug, Deserialize, Clone)]
pub struct Filter {
    #[serde(default)]
    pub allow_types: Option<Vec<String>>,

    #[serde(default)]
    pub allow_functions: Option<Vec<String>>,

    #[serde(default)]
    pub allow_vars: Option<Vec<String>>,
    
    #[serde(default)]
    pub block_types: Option<Vec<String>>,

    #[serde(default)]
    pub block_functions: Option<Vec<String>>,
    
    #[serde(default)]
    pub block_vars: Option<Vec<String>>,

    #[serde(default)]
    pub block_files: Option<Vec<String>>
}

/// A builder for creating a [`bindgen::Builder`].
#[derive(Clone, Default, Debug)]
#[must_use]
pub struct Factory {
    pub clang_args: Vec<String>,
    pub linker: Option<PathBuf>,
    pub mcu: Option<String>,
    pub force_cpp: bool,
    pub sysroot: Option<PathBuf>,
}

impl Factory {
    /// Create a new factory populating the clang args, linker and mcu from the
    /// Scons variables of a platformio project.
    #[cfg(feature = "pio")]
    pub fn from_scons_vars(scons_vars: &crate::pio::project::SconsVariables) -> Result<Self> {
        use crate::cli;
        let clang_args = cli::NativeCommandArgs::new(&scons_vars.incflags)
            .chain(cli::NativeCommandArgs::new(
                scons_vars.clangargs.as_deref().unwrap_or_default(),
            ))
            .collect();

        Ok(Self {
            clang_args,
            linker: Some(scons_vars.full_path(scons_vars.link.clone())?),
            mcu: Some(scons_vars.mcu.clone()),
            force_cpp: false,
            sysroot: None,
        })
    }

    /// Create a new factory populating the clang args, force cpp, and sysroot from the
    /// cmake file-api compile group.
    #[cfg(feature = "cmake")]
    pub fn from_cmake(
        compile_group: &crate::cmake::file_api::codemodel::target::CompileGroup,
    ) -> Result<Self> {
        use crate::cmake::file_api::codemodel::Language;
        assert!(
            compile_group.language == Language::C || compile_group.language == Language::Cpp,
            "Generating bindings for languages other than C/C++ is not supported"
        );

        let clang_args = compile_group
            .defines
            .iter()
            .map(|d| format!("-D{}", d.define))
            .chain(
                compile_group
                    .includes
                    .iter()
                    .map(|i| format!("-I{}", &i.path)),
            )
            .collect();

        Ok(Self {
            clang_args,
            linker: None,
            force_cpp: compile_group.language == Language::Cpp,
            mcu: None,
            sysroot: compile_group.sysroot.as_ref().map(|s| s.path.clone()),
        })
    }

    pub fn new() -> Self {
        Default::default()
    }

    /// Set the clang args that need to be passed down to the Bindgen instance.
    pub fn with_clang_args<S>(mut self, clang_args: impl IntoIterator<Item = S>) -> Self
    where
        S: Into<String>,
    {
        self.clang_args
            .extend(clang_args.into_iter().map(Into::into));
        self
    }

    /// Set the sysroot to be used for generating bindings.
    pub fn with_sysroot(mut self, sysroot: impl Into<PathBuf>) -> Self {
        self.sysroot = Some(sysroot.into());
        self
    }

    /// Set the linker used to determine the sysroot to be used for generating bindings, if the sysroot is not explicitly passed.
    pub fn with_linker(mut self, linker: impl Into<PathBuf>) -> Self {
        self.linker = Some(linker.into());
        self
    }

    /// Create a [`bindgen::Builder`] with these settings.
    pub fn builder(self) -> Result<bindgen::Builder> {
        self.create_builder(false, None)
    }

    /// Create a [`bindgen::Builder`] creating C++ bindings with these settings.
    pub fn cpp_builder(self) -> Result<bindgen::Builder> {
        self.create_builder(true, None)
    }

    pub fn create_builder(self, cpp: bool, filter: Option<Filter>) -> Result<bindgen::Builder> {
        let cpp = self.force_cpp || cpp;
        let sysroot = self
            .sysroot
            .clone()
            .map_or_else(|| try_get_sysroot(&self.linker), Ok)?;

        let sysroot_args = [
            format!("--sysroot={}", sysroot.try_to_str()?),
            format!("-I{}", sysroot.join("include").try_to_str()?),
        ];

        let cpp_args = if cpp {
            get_cpp_includes(&sysroot)?
        } else {
            vec![]
        };

        let mut builder = bindgen::Builder::default()
            .use_core()
            .layout_tests(false)
            .formatter(bindgen::Formatter::None)
            .derive_default(true)
            .clang_arg("-D__bindgen")
            // Include directories provided by the build system
            // should be first on the search path (before sysroot includes),
            // or else libc's <dirent.h> does not correctly override sysroot's <dirent.h>
            .clang_args(&self.clang_args)
            .clang_args(sysroot_args)
            .clang_args(&["-x", if cpp { "c++" } else { "c" }])
            .clang_args(cpp_args)
            .generate_inline_functions(cpp).clang_arg("-fno-inline-functions");

        if let Some(filter) = filter {
            if let Some(allow_functions) = filter.allow_functions {
                for allow_function in allow_functions {
                    builder = builder.allowlist_function(allow_function);
                }
            }
            if let Some(allow_types) = filter.allow_types {
                for allow_type in allow_types {
                    builder = builder.allowlist_type(allow_type);
                }
            }
            if let Some(allow_vars) = filter.allow_vars {
                for allow_var in allow_vars {
                    builder = builder.allowlist_var(allow_var);
                }
            }
            if let Some(block_types) = filter.block_types {
                for block_type in block_types {
                    builder = builder.blocklist_type(block_type);
                }
            }
            if let Some(block_functions) = filter.block_functions {
                for block_function in block_functions {
                    builder = builder.blocklist_function(block_function);
                }
            }
            if let Some(block_vars) = filter.block_vars {
                for block_var in block_vars {
                    builder = builder.blocklist_var(block_var);
                }
            }
            if let Some(block_files) = filter.block_files {
                for block_file in block_files {
                    builder = builder.blocklist_file(block_file);
                }
            }
        }

        log::debug!(
            "Bindgen builder factory flags: {:?}",
            builder.command_line_flags()
        );

        Ok(builder)
    }
}

/// Get the default filename for bindings and set the environment variable named
/// [`VAR_BINDINGS_FILE`] that is available during crate compilation to that path.
pub fn default_bindings_file() -> Result<PathBuf> {
    let bindings_file = out_dir().join("bindings.rs");
    cargo::set_rustc_env(VAR_BINDINGS_FILE, bindings_file.try_to_str()?);
    Ok(bindings_file)
}

/// Create rust bindings in [`default_bindings_file`] using [`run_for_file`].
pub fn run(builder: bindgen::Builder) -> Result<PathBuf> {
    let output_file = default_bindings_file()?;
    run_for_file(builder, &output_file)?;
    Ok(output_file)
}

/// Try to `cargo fmt` `file` using any of the current, `stable` and `nightly` toolchains.
/// If all of them fail print a warning ([`cargo::print_warning`]).
pub fn cargo_fmt_file(file: impl AsRef<Path>) {
    let file = file.as_ref();
    // Run rustfmt on the generated bindings separately, because custom toolchains often do not have rustfmt
    // We try multiple rustfmt instances:
    // - The one from the currently active toolchain
    // - The one from stable
    // - The one from nightly
    if cmd!("rustfmt", file).run().is_err()
        && cmd!("rustup", "run", "stable", "rustfmt", file)
            .run()
            .is_err()
        && cmd!("rustup", "run", "nightly", "rustfmt", file)
            .run()
            .is_err()
    {
        cargo::print_warning(
            "rustfmt not found in the current toolchain, nor in stable or nightly. \
             The generated bindings will not be properly formatted.",
        );
    }
}

/// Create rust bindings in `output_file` and run `cargo fmt` over that file.
pub fn run_for_file(builder: bindgen::Builder, output_file: impl AsRef<Path>) -> Result<()> {
    let output_file = output_file.as_ref();

    eprintln!("Output: {output_file:?}");
    eprintln!("Bindgen builder flags: {:?}", builder.command_line_flags());

    let bindings = builder
        .generate()
        .map_err(|_| Error::msg("Failed to generate bindings"))?;

    bindings.write_to_file(output_file)?;
    cargo_fmt_file(output_file);

    Ok(())
}

/// Extension trait for [`bindgen::Builder`].
pub trait BindgenExt: Sized {
    /// Add all input C/C++ headers using repeated [`bindgen::Builder::header`].
    fn headers(self, headers: impl IntoIterator<Item = impl AsRef<Path>>) -> Result<Self>;
}

impl BindgenExt for bindgen::Builder {
    fn headers(mut self, headers: impl IntoIterator<Item = impl AsRef<Path>>) -> Result<Self> {
        for header in headers {
            self = self.header(header.as_ref().try_to_str()?)
        }
        Ok(self)
    }
}

fn try_get_sysroot(linker: &Option<impl AsRef<Path>>) -> Result<PathBuf> {
    let linker = if let Some(ref linker) = linker {
        linker.as_ref().to_owned()
    } else if let Some(linker) = env::var_os("RUSTC_LINKER") {
        PathBuf::from(linker)
    } else {
        bail!("Could not determine linker: No explicit linker and `RUSTC_LINKER` not set");
    };

    let gcc_file_stem = linker
        .file_stem()
        .and_then(OsStr::to_str)
        .filter(|&s| s == "gcc" || s.ends_with("-gcc"));

    // For whatever reason, --print-sysroot does not work with GCC
    // Change it to LD
    let linker = if let Some(stem) = gcc_file_stem {
        let mut ld_linker =
            linker.with_file_name(format!("{}{}", stem.strip_suffix("gcc").unwrap(), "ld"));
        if let Some(ext) = linker.extension() {
            ld_linker.set_extension(ext);
        }
        ld_linker
    } else {
        linker
    };

    cmd!(&linker, "--print-sysroot")
        .stdout()
        .with_context(|| {
            anyhow!(
                "Could not determine sysroot from linker '{}'",
                linker.display()
            )
        })
        .map(PathBuf::from)
}

fn get_cpp_includes(sysroot: impl AsRef<Path>) -> Result<Vec<String>> {
    let sysroot = sysroot.as_ref();
    let cpp_includes_root = sysroot.join("include").join("c++");

    let cpp_version = fs::read_dir(cpp_includes_root)?
        .map(|dir_entry_r| dir_entry_r.map(|dir_entry| dir_entry.path()))
        .fold(None, |ao: Option<PathBuf>, sr: Result<PathBuf, _>| {
            if let Some(a) = ao.as_ref() {
                sr.ok()
                    .map_or(ao.clone(), |s| if a >= &s { ao.clone() } else { Some(s) })
            } else {
                sr.ok()
            }
        });

    if let Some(cpp_version) = cpp_version {
        let mut cpp_include_paths = vec![
            format!("-I{}", cpp_version.try_to_str()?),
            format!("-I{}", cpp_version.join("backward").try_to_str()?),
        ];

        if let Some(sysroot_last_segment) = fs::canonicalize(sysroot)?.file_name() {
            cpp_include_paths.push(format!(
                "-I{}",
                cpp_version.join(sysroot_last_segment).try_to_str()?
            ));
        }

        Ok(cpp_include_paths)
    } else {
        Ok(Vec::new())
    }
}
