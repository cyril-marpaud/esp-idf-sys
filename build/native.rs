//! Install tools and build the `esp-idf` using native tooling.

use std::convert::TryFrom;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::{env, fs};

use anyhow::*;
use embuild::cmake::file_api::codemodel::Language;
use embuild::cmake::file_api::ObjKind;
use embuild::fs::copy_file_if_different;
use embuild::utils::{OsStrExt, PathExt};
use embuild::{bindgen, build, cargo, cmake, espidf, git, kconfig, path_buf};
use strum::{Display, EnumString};

use super::common::{
    self, get_sdkconfig_profile, workspace_dir, EspIdfBuildOutput, EspIdfComponents,
    InstallLocation, ESP_IDF_GLOB_VAR_PREFIX, ESP_IDF_SDKCONFIG_DEFAULTS_VAR,
    ESP_IDF_SDKCONFIG_VAR, ESP_IDF_TOOLS_INSTALL_DIR_VAR, MASTER_PATCHES, MCU_VAR, STABLE_PATCHES,
};

const ESP_IDF_VERSION_VAR: &str = "ESP_IDF_VERSION";
const ESP_IDF_REPOSITORY_VAR: &str = "ESP_IDF_REPOSITORY";

const DEFAULT_ESP_IDF_VERSION: &str = "v4.3.1";

const CARGO_CMAKE_BUILD_ACTIVE_VAR: &str = "CARGO_CMAKE_BUILD_ACTIVE";
const CARGO_CMAKE_BUILD_INCLUDES_VAR: &str = "CARGO_CMAKE_BUILD_INCLUDES";
const CARGO_CMAKE_BUILD_LINK_LIBRARIES_VAR: &str = "CARGO_CMAKE_BUILD_LINK_LIBRARIES";
const CARGO_CMAKE_BUILD_COMPILER_VAR: &str = "CARGO_CMAKE_BUILD_COMPILER";
const CARGO_CMAKE_BUILD_SDKCONFIG_VAR: &str = "CARGO_CMAKE_BUILD_SDKCONFIG";

pub fn build() -> Result<EspIdfBuildOutput> {
    if env::var(CARGO_CMAKE_BUILD_ACTIVE_VAR).is_ok()
        || env::var(CARGO_CMAKE_BUILD_INCLUDES_VAR).is_ok()
    {
        build_cmake_first()
    } else {
        build_cargo_first()
    }
}

fn build_cmake_first() -> Result<EspIdfBuildOutput> {
    let components = EspIdfComponents::from(
        env::var(CARGO_CMAKE_BUILD_LINK_LIBRARIES_VAR)?
            .split(';')
            .filter_map(|s| {
                s.strip_prefix("__idf_").map(|comp| {
                    // All ESP-IDF components are prefixed with `__idf_`
                    // Check this comment for more info:
                    // https://github.com/esp-rs/esp-idf-sys/pull/17#discussion_r723133416
                    format!("comp_{}_enabled", comp)
                })
            }),
    );

    let sdkconfig = PathBuf::from(env::var(CARGO_CMAKE_BUILD_SDKCONFIG_VAR)?);

    let build_output = EspIdfBuildOutput {
        cincl_args: build::CInclArgs {
            args: env::var(CARGO_CMAKE_BUILD_INCLUDES_VAR)?,
        },
        link_args: None,
        kconfig_args: Box::new(
            kconfig::try_from_config_file(sdkconfig.clone())
                .with_context(|| anyhow!("Failed to read '{:?}'", sdkconfig))?
                .map(|(key, value)| {
                    (
                        key.strip_prefix("CONFIG_")
                            .map(str::to_string)
                            .unwrap_or(key),
                        value,
                    )
                }),
        ),
        components,
        bindgen: bindgen::Factory::new()
            .with_linker(env::var(CARGO_CMAKE_BUILD_COMPILER_VAR)?)
            .with_clang_args(
                env::var(CARGO_CMAKE_BUILD_INCLUDES_VAR)?
                    .split(';')
                    .map(|dir| format!("-I{}", dir))
                    .collect::<Vec<_>>(),
            ),
    };

    Ok(build_output)
}

fn build_cargo_first() -> Result<EspIdfBuildOutput> {
    let out_dir = cargo::out_dir();
    let target = env::var("TARGET")?;
    let workspace_dir = workspace_dir()?;
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR")?);

    let chip = if let Some(mcu) = env::var_os(MCU_VAR) {
        Chip::from_str(&mcu.to_string_lossy())?
    } else {
        Chip::detect(&target)?
    };
    let chip_name = chip.to_string();
    let profile = common::build_profile();

    cargo::track_env_var(espidf::IDF_PATH_VAR);
    cargo::track_env_var(ESP_IDF_TOOLS_INSTALL_DIR_VAR);
    cargo::track_env_var(ESP_IDF_VERSION_VAR);
    cargo::track_env_var(ESP_IDF_REPOSITORY_VAR);
    cargo::track_env_var(ESP_IDF_SDKCONFIG_DEFAULTS_VAR);
    cargo::track_env_var(ESP_IDF_SDKCONFIG_VAR);
    cargo::track_env_var(MCU_VAR);

    let idf_frompath = espidf::EspIdf::try_from_env()?;

    let install_location = InstallLocation::get("espressif")?;

    if idf_frompath.is_some()
        && install_location.is_some()
        && !matches!(install_location, Some(InstallLocation::FromPath))
    {
        bail!(
            "ESP-IDF tooling detected on path, however user has overriden with `{}` via `{}`",
            install_location.unwrap(),
            InstallLocation::get_env_var_name()
        );
    }

    let idf = if let Some(idf_frompath) = idf_frompath {
        match install_location {
            None | Some(InstallLocation::FromPath) => idf_frompath,
            Some(install_location) => bail!(
                "Install location is configured to `{}` via `{}`, however there is ESP-IDF tooling detected on path", 
                install_location,
                InstallLocation::get_env_var_name()),
        }
    } else {
        let install_location = install_location
            .map(Result::Ok)
            .unwrap_or_else(|| InstallLocation::new_workspace("espressif"))?;

        if matches!(install_location, InstallLocation::FromPath) {
            bail!(
                "Install location is configured to `{}` via `{}`, however no ESP-IDF tooling was detected on path", 
                InstallLocation::FromPath,
                InstallLocation::get_env_var_name());
        }

        let cmake_tool = espidf::Tools::cmake()?;
        let tools = espidf::Tools::new(
            vec!["ninja", chip.gcc_toolchain()]
                .into_iter()
                .chain(chip.ulp_gcc_toolchain()),
        );

        let idf_reference = esp_idf_reference()?;

        let idf = espidf::Installer::new(idf_reference.clone())
            .install_dir(install_location.path().map(PathBuf::from))
            .with_tools(tools)
            .with_tools(cmake_tool)
            .install()
            .context("Could not install esp-idf")?;

        // Apply patches, only if the patches were not previously applied and only if the used esp-idf instance is managed.
        if let espidf::EspIdfReference::Managed(managed) = &idf_reference {
            let patch_set = match &managed.git_ref {
                git::Ref::Branch(ref b)
                    if idf.repository.get_default_branch()?.as_ref() == Some(b) =>
                {
                    MASTER_PATCHES
                }
                git::Ref::Tag(t) if t == DEFAULT_ESP_IDF_VERSION => STABLE_PATCHES,
                _ => {
                    cargo::print_warning(format_args!(
                        "`esp-idf` version ({:?}) not officially supported by `esp-idf-sys`. \
                        Supported versions are 'master', '{}'.",
                        managed.git_ref, DEFAULT_ESP_IDF_VERSION
                    ));
                    &[]
                }
            };
            if !patch_set.is_empty() {
                idf.repository
                    .apply_once(patch_set.iter().map(|p| manifest_dir.join(p)))?;
            }
        }

        idf
    };

    env::set_var("PATH", &idf.exported_path);

    // Create cmake project.
    copy_file_if_different(
        manifest_dir.join(path_buf!("resources", "cmake_project", "CMakeLists.txt")),
        &out_dir,
    )?;
    copy_file_if_different(
        manifest_dir.join(path_buf!("resources", "cmake_project", "main.c")),
        &out_dir,
    )?;

    // Copy additional globbed files specified by user env variables
    for file in build::tracked_env_globs_iter(ESP_IDF_GLOB_VAR_PREFIX)? {
        let dest_path = out_dir.join(file.1);
        fs::create_dir_all(dest_path.parent().unwrap())?;
        // TODO: Maybe warn if this overwrites a critical file (e.g. CMakeLists.txt).
        // It could be useful for the user to explicitly overwrite our files.
        copy_file_if_different(&file.0, &out_dir)?;
    }

    // The `kconfig.cmake` script looks at this variable if it should compile `mconf` on windows.
    // But this variable is also present when using git-bash which doesn't have gcc.
    env::remove_var("MSYSTEM");

    // Resolve `ESP_IDF_SDKCONFIG` and `ESP_IDF_SDKCONFIG_DEFAULTS` to an absolute path
    // relative to the workspace directory if not empty.
    let sdkconfig = env::var_os(ESP_IDF_SDKCONFIG_VAR)
        .map(|s| {
            let path = Path::new(&s).abspath_relative_to(&workspace_dir);
            let path = get_sdkconfig_profile(&path, &profile, &chip_name).unwrap_or(path);
            cargo::track_file(&path);
            path
        })
        .filter(|v| v.exists());
    let sdkconfig_defaults = {
        let gen_defaults_path = out_dir.join("gen-sdkconfig.defaults");
        fs::write(&gen_defaults_path, generate_sdkconfig_defaults()?)?;

        let mut result = vec![gen_defaults_path];
        result.extend(
            env::var_os(ESP_IDF_SDKCONFIG_DEFAULTS_VAR)
                .unwrap_or_default()
                .try_to_str()?
                .split(';')
                .filter_map(|v| {
                    if !v.is_empty() {
                        let path = Path::new(v).abspath_relative_to(&workspace_dir);
                        cargo::track_file(&path);
                        Some(path)
                    } else {
                        None
                    }
                }),
        );
        result
    };
    let defaults_files = sdkconfig_defaults
        .iter()
        // Use the `sdkconfig` as a defaults file to prevent it from being changed by the
        // build. It must be the last defaults file so that its options have precendence
        // over any actual defaults from files before it.
        .chain(sdkconfig.as_ref())
        .try_fold(OsString::new(), |mut accu, p| -> Result<OsString> {
            if !accu.is_empty() {
                accu.push(";");
            }
            // Windows uses `\` as directory separators which cmake can't deal with, so we
            // convert all back-slashes to forward-slashes here. This would be tedious to
            // do with an `OsString` so we have to convert it to `str` first.
            if cfg!(windows) {
                accu.push(p.try_to_str()?.replace('\\', "/"));
            } else {
                accu.push(p);
            }
            Ok(accu)
        })?;

    let cmake_toolchain_file = path_buf![
        &idf.repository.worktree(),
        "tools",
        "cmake",
        chip.cmake_toolchain_file()
    ];

    // Get the asm, C and C++ flags from the toolchain file, these would otherwise get
    // overwritten because `cmake::Config` also sets these (see
    // https://github.com/espressif/esp-idf/issues/7507).
    let (asm_flags, c_flags, cxx_flags) = {
        let mut vars = cmake::get_script_variables(&cmake_toolchain_file)?;
        (
            vars.remove("CMAKE_ASM_FLAGS").unwrap_or_default(),
            vars.remove("CMAKE_C_FLAGS").unwrap_or_default(),
            vars.remove("CMAKE_CXX_FLAGS").unwrap_or_default(),
        )
    };

    // `cmake::Config` automatically uses `<out_dir>/build` and there is no way to query
    // what build directory it sets, so we hard-code it.
    let cmake_build_dir = out_dir.join("build");

    let query = cmake::Query::new(
        &cmake_build_dir,
        "cargo",
        &[ObjKind::Codemodel, ObjKind::Toolchains],
    )?;

    // Build the esp-idf.
    cmake::Config::new(&out_dir)
        .generator("Ninja")
        .out_dir(&out_dir)
        .no_build_target(true)
        .define("CMAKE_TOOLCHAIN_FILE", &cmake_toolchain_file)
        .define("CMAKE_BUILD_TYPE", "")
        .always_configure(true)
        .pic(false)
        .asmflag(asm_flags)
        .cflag(c_flags)
        .cxxflag(cxx_flags)
        .env("IDF_PATH", &idf.repository.worktree())
        .env("PATH", &idf.exported_path)
        .env("SDKCONFIG_DEFAULTS", defaults_files)
        .env("IDF_TARGET", &chip_name)
        .build();

    let replies = query.get_replies()?;
    let target = replies
        .get_codemodel()?
        .into_first_conf()
        .get_target("libespidf.elf")
        .unwrap_or_else(|| {
            bail!("Could not read build information from cmake: Target 'libespidf.elf' not found")
        })?;

    let compiler = replies
        .get_toolchains()
        .and_then(|mut t| {
            t.take(Language::C)
                .ok_or_else(|| Error::msg("No C toolchain"))
        })
        .and_then(|t| {
            t.compiler
                .path
                .ok_or_else(|| Error::msg("No compiler path set"))
        })
        .context("Could not determine the compiler from cmake")?;

    let sdkconfig_json = path_buf![&cmake_build_dir, "config", "sdkconfig.json"];

    let build_output = EspIdfBuildOutput {
        cincl_args: build::CInclArgs::try_from(&target.compile_groups[0])?,
        link_args: Some(
            build::LinkArgsBuilder::try_from(&target.link.unwrap())?
                .linker(&compiler)
                .working_directory(&cmake_build_dir)
                .force_ldproxy(true)
                .build()?,
        ),
        bindgen: bindgen::Factory::from_cmake(&target.compile_groups[0])?.with_linker(&compiler),
        components: EspIdfComponents::new(),
        kconfig_args: Box::new(
            kconfig::try_from_json_file(sdkconfig_json.clone())
                .with_context(|| anyhow!("Failed to read '{:?}'", sdkconfig_json))?,
        ),
    };

    Ok(build_output)
}

fn esp_idf_reference() -> Result<espidf::EspIdfReference> {
    let unmanaged = match env::var(espidf::IDF_PATH_VAR) {
        Err(env::VarError::NotPresent) => None,
        v => Some(git::Repository::open(v?)?),
    };

    let managed_repo_url = match env::var(ESP_IDF_REPOSITORY_VAR) {
        Err(env::VarError::NotPresent) => None,
        git_url => Some(git_url?),
    };

    let managed_version = match env::var(ESP_IDF_VERSION_VAR) {
        Err(env::VarError::NotPresent) => None,
        v => Some(v?),
    };

    let reference = if let Some(unmanaged) = unmanaged {
        if managed_repo_url.is_some() {
            println!(
                "cargo:warning=User-supplied ESP-IDF detected via {}, setting `{}` will be ignored",
                espidf::IDF_PATH_VAR,
                ESP_IDF_REPOSITORY_VAR
            );
        }

        if managed_version.is_some() {
            println!(
                "cargo:warning=User-supplied ESP-IDF detected via {}, setting `{}` will be ignored",
                espidf::IDF_PATH_VAR,
                ESP_IDF_VERSION_VAR
            );
        }

        espidf::EspIdfReference::Unmanaged(unmanaged)
    } else {
        let managed_version = managed_version
            .as_deref()
            .unwrap_or(DEFAULT_ESP_IDF_VERSION);

        espidf::EspIdfReference::Managed(espidf::Managed {
            repo_url: managed_repo_url,
            git_ref: espidf::Managed::decode_version_ref(managed_version),
        })
    };

    Ok(reference)
}

// Generate `sdkconfig.defaults` content based on the crate manifest (`Cargo.toml`).
//
// This is currently only used to forward the optimization options to the esp-idf.
fn generate_sdkconfig_defaults() -> Result<String> {
    const OPT_VARS: [&str; 4] = [
        "CONFIG_COMPILER_OPTIMIZATION_NONE",
        "CONFIG_COMPILER_OPTIMIZATION_DEFAULT",
        "CONFIG_COMPILER_OPTIMIZATION_PERF",
        "CONFIG_COMPILER_OPTIMIZATION_SIZE",
    ];

    let opt_level = env::var("OPT_LEVEL")?;
    let debug = env::var("DEBUG")?;
    let opt_index = match (opt_level.as_str(), debug.as_str()) {
        ("s" | "z", _) => 3,               // -Os
        ("1", _) | (_, "2" | "true") => 1, // -Og
        ("0", _) => 0,                     // -O0
        ("2" | "3", _) => 2,               // -O2
        _ => unreachable!("Invalid DEBUG or OPT_LEVEL"),
    };

    Ok(OPT_VARS
        .iter()
        .enumerate()
        .map(|(i, s)| format!("{}={}\n", s, if i == opt_index { 'y' } else { 'n' }))
        .collect::<String>())
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Display, EnumString)]
#[repr(u32)]
enum Chip {
    /// Xtensa LX7 based dual core
    #[strum(serialize = "esp32")]
    ESP32 = 0,
    /// Xtensa LX7 based single core
    #[strum(serialize = "esp32s2")]
    ESP32S2,
    /// Xtensa LX7 based single core
    #[strum(serialize = "esp32s3")]
    ESP32S3,
    /// RISC-V based single core
    #[strum(serialize = "esp32c3")]
    ESP32C3,
}

impl Chip {
    fn detect(rust_target_triple: &str) -> Result<Chip> {
        if rust_target_triple.starts_with("xtensa-esp") {
            if rust_target_triple.contains("esp32s3") {
                return Ok(Chip::ESP32S3);
            } else if rust_target_triple.contains("esp32s2") {
                return Ok(Chip::ESP32S2);
            } else {
                return Ok(Chip::ESP32);
            }
        } else if rust_target_triple.starts_with("riscv32imc-esp") {
            return Ok(Chip::ESP32C3);
        }
        bail!("Unsupported target '{}'", rust_target_triple)
    }

    /// The name of the gcc toolchain (to compile the `esp-idf`) for `idf_tools.py`.
    fn gcc_toolchain(self) -> &'static str {
        match self {
            Self::ESP32 => "xtensa-esp32-elf",
            Self::ESP32S2 => "xtensa-esp32s2-elf",
            Self::ESP32S3 => "xtensa-esp32s3-elf",
            Self::ESP32C3 => "riscv32-esp-elf",
        }
    }

    /// The name of the gcc toolchain for the ultra low-power co-processor for
    /// `idf_tools.py`.
    fn ulp_gcc_toolchain(self) -> Option<&'static str> {
        match self {
            Self::ESP32 => Some("esp32ulp-elf"),
            Self::ESP32S2 => Some("esp32s2ulp-elf"),
            _ => None,
        }
    }

    fn cmake_toolchain_file(self) -> String {
        format!("toolchain-{}.cmake", self)
    }
}
