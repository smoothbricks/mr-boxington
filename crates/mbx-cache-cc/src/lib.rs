//! Conservative parsing and action-key construction for C and C++ compiles.
//!
//! Cargo build scripts using the `cc` crate compile C and C++ through a
//! gcc-style driver. This adapter models the narrow shape those build scripts
//! produce -- one source, one object, `-c` -- plus named `-E` output. As
//! in the rustc adapter, callers should treat [`CcBypassReason`] as a safe
//! cache bypass: run the real compiler and publish nothing.
//!
//! Two properties separate this adapter from a traditional compiler cache.
//! Preprocessor inputs are discovered from a depfile the adapter injects
//! itself, so the key names the headers the compilation actually read; and the
//! directories those headers were searched from contribute a name manifest, so
//! a header that newly *shadows* one of them changes the key even though every
//! previously-read file is byte-identical.
//!
//! Path mappings are shared with the rustc adapter so both agree on which host
//! roots are checkout-specific.
#![deny(missing_docs)]

use mbx_cache_core::{
    CacheDigest, FileDigestCache, NIX_STORE, PathAliases, PathMapping, PathNormalizationError,
    canonical_json, compact_prediction_inputs, expand_prediction_inputs,
    normalize_resolved_mapped_path_with, resolve_path_mappings,
};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::path::{Component, Path, PathBuf};
use thiserror::Error;

mod depfile;

pub use depfile::{CcDepfile, CcDiscoveredInputs, INCLUDE_MANIFEST_PREFIX, manifest_snapshot};

/// Schema version embedded in canonical cc action descriptors.
pub const ACTION_SCHEMA_VERSION: u8 = 1;
/// Version of the cc argument and input model used to construct keys.
pub const ADAPTER_VERSION: u8 = 1;

/// Maximum discovered inputs, including include-manifest entries.
pub const MAX_PREDICTED_INPUTS: usize = 16 * 1024;
/// Maximum total bytes digested for one action.
pub const MAX_INPUT_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// Maximum file names summarized across all include manifests.
pub const MAX_MANIFEST_ENTRIES: usize = 16 * 1024;

/// Environment variables whose values enter every cc action key.
///
/// These change the compiler's own behavior without appearing in argv. They
/// are recorded even when unset, so setting one is distinguishable from
/// leaving it unset.
pub const KEYED_ENVIRONMENT: &[&str] = &[
    "IPHONEOS_DEPLOYMENT_TARGET",
    "LANG",
    "LC_ALL",
    "LC_MESSAGES",
    "MACOSX_DEPLOYMENT_TARGET",
    "SDKROOT",
    "SOURCE_DATE_EPOCH",
    "TVOS_DEPLOYMENT_TARGET",
    "WATCHOS_DEPLOYMENT_TARGET",
    "XROS_DEPLOYMENT_TARGET",
];

/// Environment variables that force a bypass when set.
///
/// Each one either injects search paths the argv model cannot see, redirects
/// sub-tool resolution beneath the identity probe, or makes the driver write an
/// output the adapter does not model.
pub const BYPASS_ENVIRONMENT: &[&str] = &[
    "CPATH",
    "COMPILER_PATH",
    "CPLUS_INCLUDE_PATH",
    "C_INCLUDE_PATH",
    "DEPENDENCIES_OUTPUT",
    "GCC_EXEC_PREFIX",
    "OBJC_INCLUDE_PATH",
    "SUNPRO_DEPENDENCIES",
];

/// Variable families a Nix cc-wrapper reads to build the command line it hands
/// the compiler it wraps.
///
/// nixpkgs' `cc-wrapper.sh` prepends `NIX_CFLAGS_COMPILE_BEFORE` and appends
/// `NIX_CFLAGS_COMPILE` to every compile, adds `NIX_CXXSTDLIB_COMPILE` to C++
/// ones, turns `NIX_HARDENING_ENABLE` into flags such as
/// `-fstack-protector-strong` and `-D_FORTIFY_SOURCE=3`, drops `-m*=native`
/// under `NIX_ENFORCE_NO_NATIVE`, and on Darwin derives its SDK and minimum OS
/// version from `DEVELOPER_DIR` and the deployment target. It reads each one
/// under its own name, under a role suffix (`_FOR_BUILD`, `_FOR_TARGET`) and
/// under its target salt (`_arm64_apple_darwin`), and the `NIX_CC_WRAPPER_*`
/// and `NIX_BINTOOLS_WRAPPER_*` variables pick which of those it takes.
/// `NIX_DEBUG` adds the wrapper's own lines to the standard error a hit
/// replays. None of it shows in argv or in the `-v` banner the identity probe
/// reads, which the wrapper answers for the wrapped compiler alone. Every
/// platform's deployment target is a family here, not only the one this
/// wrapper mangles today.
///
/// A variable is in a family when its name is the family's or extends it with
/// `_`; every one that is set enters the key with its value. The wrapper reads
/// its link variables (`NIX_LDFLAGS`, `NIX_CFLAGS_LINK`, ...) only when it
/// links, which a `-c` or `-E` invocation never does, so they stay out.
pub const NIX_CC_WRAPPER_ENVIRONMENT: &[&str] = &[
    "DEVELOPER_DIR",
    "IPHONEOS_DEPLOYMENT_TARGET",
    "MACOSX_DEPLOYMENT_TARGET",
    "NIX_BINTOOLS_WRAPPER",
    "NIX_CC_WRAPPER",
    "NIX_CFLAGS_COMPILE",
    "NIX_CXXSTDLIB_COMPILE",
    "NIX_DEBUG",
    "NIX_ENFORCE_NO_NATIVE",
    "NIX_HARDENING_ENABLE",
    "TVOS_DEPLOYMENT_TARGET",
    "WATCHOS_DEPLOYMENT_TARGET",
    "XROS_DEPLOYMENT_TARGET",
];

/// The families whose words a Nix cc-wrapper puts on the compiler's command
/// line.
const NIX_CC_WRAPPER_FLAGS: &[&str] = &["NIX_CFLAGS_COMPILE", "NIX_CXXSTDLIB_COMPILE"];

/// Absolute roots whose contents are keyed verbatim rather than through a
/// placeholder.
///
/// Files beneath these roots are still digested; keying the path verbatim only
/// declares that the path itself is a machine property rather than a
/// checkout-specific one, which is what makes system headers shareable between
/// worktrees on one machine.
pub const SYSTEM_ROOTS: &[&str] = &[
    "/Applications/Xcode.app",
    "/Library/Developer",
    NIX_STORE,
    "/usr/include",
    "/usr/lib",
    "/usr/local/include",
];

/// `-f` options whose text describes them completely: none names a file or
/// writes a second output, so each enters the key verbatim, its value
/// included. Some only shape diagnostics (`error-limit`), and they are keyed
/// all the same, since a hit replays the diagnostics too.
///
/// Sorted, for the `binary_search` in `parse_f_flag`.
const SUPPORTED_F_FLAGS: &[&str] = &[
    "PIC",
    "PIE",
    "asynchronous-unwind-tables",
    "cf-protection",
    "color-diagnostics",
    "constexpr-depth",
    "constexpr-steps",
    "data-sections",
    "diagnostics-color",
    "error-limit",
    "exceptions",
    "fast-math",
    "finite-math-only",
    "function-sections",
    "lto",
    "math-errno",
    "merge-all-constants",
    "no-asynchronous-unwind-tables",
    "no-builtin",
    "no-c++-static-destructors",
    "no-common",
    "no-exceptions",
    "no-fast-math",
    "no-finite-math-only",
    "no-lto",
    "no-math-errno",
    "no-omit-frame-pointer",
    "no-pic",
    "no-pie",
    "no-plt",
    "no-reciprocal-math",
    "no-rtti",
    "no-semantic-interposition",
    "no-stack-protector",
    "no-strict-aliasing",
    "no-tree-vectorize",
    "no-unroll-loops",
    "no-unwind-tables",
    "omit-frame-pointer",
    "pic",
    "pie",
    // Seeds names GCC makes up, such as those of anonymous namespaces; nixpkgs
    // passes one derived from the output path in `NIX_CFLAGS_COMPILE`.
    "random-seed",
    "reciprocal-math",
    "rtti",
    "sanitize-undefined-strip-path-components",
    "short-enums",
    "signed-char",
    "stack-clash-protection",
    "stack-protector",
    "stack-protector-all",
    "stack-protector-strong",
    "strict-aliasing",
    "tls-model",
    "tree-vectorize",
    "unroll-loops",
    "unsigned-char",
    "unwind-tables",
    "visibility",
    "visibility-inlines-hidden",
    "wrapv",
];

const SUPPORTED_M_FLAGS: &[&str] = &[
    "32",
    "64",
    "adx",
    "aes",
    "arch",
    "arm",
    "avx",
    "avx2",
    "avx512bf16",
    "avx512bitalg",
    "avx512bw",
    "avx512cd",
    "avx512dq",
    "avx512f",
    "avx512fp16",
    "avx512ifma",
    "avx512vbmi",
    "avx512vbmi2",
    "avx512vl",
    "avx512vnni",
    "avx512vpopcntdq",
    "avxvnni",
    "bmi",
    "bmi2",
    "cpu",
    "crc",
    "crypto",
    "dotprod",
    "f16c",
    "float-abi",
    "fma",
    "fpu",
    "gfni",
    "iphoneos-version-min",
    "lse",
    "lzcnt",
    "macosx-version-min",
    "movbe",
    "no-avx",
    "no-avx2",
    "no-avx512f",
    "no-omit-leaf-frame-pointer",
    "no-outline-atomics",
    "no-red-zone",
    "no-sse",
    "no-sse2",
    "omit-leaf-frame-pointer",
    "outline-atomics",
    "pclmul",
    "popcnt",
    "rdrnd",
    "rdseed",
    "red-zone",
    "sha",
    "sha512",
    "soft-float",
    "sse",
    "sse2",
    "sse3",
    "sse4.1",
    "sse4.2",
    "sse4a",
    "ssse3",
    "sve",
    "sve2",
    "thumb",
    "tune",
    "vaes",
    "vpclmulqdq",
    "xsave",
    "xsavec",
    "xsaveopt",
    "xsaves",
];

const SUPPORTED_O_FLAGS: &[&str] = &[
    "-O", "-O0", "-O1", "-O2", "-O3", "-Ofast", "-Og", "-Os", "-Oz",
];

/// `-g` spellings: whether to emit debug information, at what level, in
/// which DWARF version, and tuned for which debugger (`-ggdb`, `-glldb`).
/// Each changes only the debug information the object carries.
const SUPPORTED_G_FLAGS: &[&str] = &[
    "-g",
    "-g0",
    "-g1",
    "-g2",
    "-g3",
    "-gdwarf-2",
    "-gdwarf-3",
    "-gdwarf-4",
    "-gdwarf-5",
    "-gfull",
    "-ggdb",
    "-ggdb0",
    "-ggdb1",
    "-ggdb2",
    "-ggdb3",
    "-glldb",
];

const SUPPORTED_BARE_FLAGS: &[&str] = &[
    "-P", // Suppress preprocessor line markers; keyed even without -E.
    "-ansi",
    "-nostdinc",
    "-nostdinc++",
    "-pedantic",
    "-pedantic-errors",
    "-pipe",
    "-pthread",
    "-w",
];

const SEPARATE_PATH_FLAGS: &[&str] = &[
    "-idirafter",
    "-imacros",
    "-include",
    "-iquote",
    "-isysroot",
    "-isystem",
];

const TOOL_PASSTHROUGH_FLAGS: &[&str] = &["-Xassembler", "-Xclang", "-Xlinker", "-Xpreprocessor"];

/// Assembler options whose effects are completely described by their text.
///
/// Other assembler options can name files that dependency discovery does not
/// report, so they remain conservative passthrough bypasses.
const SUPPORTED_ASSEMBLER_OPTIONS: &[&str] = &["--noexecstack"];

const COMPILER_QUERY_FLAGS: &[&str] = &[
    "--help",
    "--version",
    "-###",
    // The `cc` crate probes with `-?` to tell an MSVC-style driver from a
    // gcc-style one; neither answer is a compilation.
    "-?",
    "-dumpmachine",
    "-dumpversion",
    "-v",
];

/// Flags that rewrite a path prefix in the compiler's own output.
///
/// The left side is a real path and normalizes like any other; the right side
/// is the text it is replaced with and enters the key verbatim.
const PREFIX_MAP_FLAGS: &[&str] = &[
    "-fdebug-prefix-map",
    "-ffile-prefix-map",
    "-fmacro-prefix-map",
];

impl CcBypassReason {
    /// A stable, low-cardinality name for this reason.
    ///
    /// Many variants carry a path or a flag, so `Display` text cannot be
    /// aggregated; statistics group by this instead.
    pub fn kind(&self) -> &'static str {
        self.into()
    }

    /// A concrete change that can make this invocation cacheable, when one is
    /// available.
    ///
    /// Expected compiler probes and failures that require adapter support
    /// return `None`; callers can still explain those from
    /// [`CcBypassReason::kind`].
    pub fn remediation(&self) -> Option<&'static str> {
        match self {
            Self::UnsupportedEnvironment(_) => Some(
                "Unset the reported environment variable for this build so the compiler invocation describes all of its inputs.",
            ),
            Self::NixImpurePath(_) => Some(
                "Keep include directories under the Nix store, `$NIX_BUILD_TOP`, or a temporary directory, or unset `NIX_ENFORCE_PURITY`, so the compiler searches what the command line names.",
            ),
            Self::LocalCpuTarget(_) => Some(
                "Replace the reported local-CPU option with an explicit architecture or CPU name.",
            ),
            Self::EmbeddedTimestampMacro(_) => Some(
                "Remove the reported timestamp macro, or keep this compilation uncached if its changing value is intentional.",
            ),
            Self::SearchPathModifiedDuringCompilation(_) => Some(
                "Generate headers before compilation instead of changing an include directory while the compiler is running.",
            ),
            Self::UnknownFlag(_) | Self::ToolPassthrough(_) => Some(
                "Upgrade mbx or report the unmodeled compiler option. If you control the build script, removing the option can also make the compilation cacheable.",
            ),
            Self::UnmappedAbsolutePath(_) => Some(
                "Move the input under a mapped project or system root, or keep this compilation uncached.",
            ),
            _ => None,
        }
    }
}

/// Reason a C or C++ invocation cannot safely use the action cache.
///
/// A bypass is an expected conservative outcome, not a compiler error. Match on
/// [`CcBypassReason::kind`] for aggregation rather than on the variants.
#[derive(Debug, Clone, PartialEq, Eq, Error, strum::IntoStaticStr)]
#[strum(serialize_all = "kebab-case")]
#[non_exhaustive]
pub enum CcBypassReason {
    /// An argument cannot be represented in the canonical UTF-8 key.
    #[error("compiler argument {index} is not valid UTF-8")]
    NonUtf8Argument {
        /// Zero-based index in the argument slice.
        index: usize,
    },
    /// The driver was handed an argument file.
    #[error("compiler response file is not modeled by the cache adapter: {0}")]
    ResponseFile(String),
    /// A compiler flag is not modeled by this adapter version.
    #[error("compiler flag is not modeled by the cache adapter: {0}")]
    UnknownFlag(String),
    /// A recognized flag was given without its value.
    #[error("compiler flag {0} is missing its value")]
    MissingValue(String),
    /// The invocation asks the driver about itself rather than compiling.
    #[error("compiler invocation queries the driver instead of compiling")]
    CompilerQuery,
    /// The invocation is not a single-object compile.
    #[error("compiler invocation does not compile with -c")]
    NotACompile,
    /// The invocation emits preprocessed source or assembly.
    #[error("compiler invocation emits a non-object output: {0}")]
    NonObjectOutput(String),
    /// The source arrives on standard input and cannot be rediscovered.
    #[error("compiler invocation reads its source from standard input")]
    StandardInput,
    /// No source file was given.
    #[error("compiler invocation names no source file")]
    MissingInput,
    /// More than one source file was given.
    #[error("compiler invocation names more than one source file")]
    MultipleInputs,
    /// No `-o` was given, so the object name follows driver defaults.
    #[error("compiler invocation names no output file")]
    MissingOutput,
    /// The source language is outside the modeled set.
    #[error("compiler input language is not modeled by the cache adapter: {0}")]
    UnsupportedLanguage(String),
    /// The caller asked for its own dependency output.
    #[error("compiler invocation requests its own dependency output: {0}")]
    CallerDependencyFlags(String),
    /// Building a precompiled header, or naming one in a spelling the adapter
    /// does not model.
    #[error("precompiled headers are not modeled by the cache adapter: {0}")]
    PrecompiledHeader(String),
    /// Coverage instrumentation writes outputs beside the object.
    #[error("coverage instrumentation is not modeled by the cache adapter: {0}")]
    CoverageInstrumentation(String),
    /// Split debug info writes a `.dwo` beside the object.
    #[error("split debug output is not modeled by the cache adapter: {0}")]
    SplitDebugOutput(String),
    /// Temporary files are preserved beside the object.
    #[error("preserved temporaries are not modeled by the cache adapter: {0}")]
    SaveTemps(String),
    /// An option is smuggled to a sub-tool the adapter cannot model.
    #[error("compiler flag forwards options to another tool: {0}")]
    ToolPassthrough(String),
    /// A compiler plugin makes the output depend on unmodeled code.
    #[error("compiler plugins are not modeled by the cache adapter: {0}")]
    Plugin(String),
    /// An include search directory gained or lost a header while the compiler
    /// ran, so the manifest recorded after it is not what the compilation saw.
    #[error("include search directory changed during the compilation: {0}")]
    SearchPathModifiedDuringCompilation(PathBuf),

    /// The object kept a path the key normalized away.
    ///
    /// Remapping covers what the compiler records itself; a path the source
    /// keeps as a string survives it, and publishing such an object would
    /// share this checkout's directory under a key that says it does not
    /// matter.
    #[error("compilation output records a path its key normalized away: {0}")]
    UnportableOutput(PathBuf),
    /// The caller chose not to store objects bound to this checkout's paths.
    #[error("storing path-specific C objects is disabled by MBX_CC_STORE_PATH_SPECIFIC")]
    PathSpecificStorageDisabled,
    /// The object depends on the machine's own CPU rather than on named inputs.
    #[error("compiler flag tunes for the local CPU: {0}")]
    LocalCpuTarget(String),
    /// The driver is not a gcc-style or clang-style compiler.
    #[error("compiler driver is not modeled by the cache adapter: {0}")]
    UnsupportedCompilerDriver(String),
    /// The identity probe could not be run or parsed.
    #[error("could not establish compiler identity: {0}")]
    CompilerIdentityUnavailable(String),
    /// An environment variable outside the modeled set is set.
    #[error("environment variable {0} changes the compilation in an unmodeled way")]
    UnsupportedEnvironment(String),
    /// A Nix cc-wrapper enforcing purity would drop this path, so the compiler
    /// would not search what the command line names.
    #[error("the Nix compiler wrapper drops this path under NIX_ENFORCE_PURITY: {0}")]
    NixImpurePath(PathBuf),
    /// The shim could not be told which real compiler to run.
    #[error("no real compiler was pinned for the cc shim")]
    RealCompilerUnpinned,
    /// A read file expands a timestamp macro, so the object is not a function
    /// of its inputs.
    #[error("input expands a timestamp macro: {0}")]
    EmbeddedTimestampMacro(PathBuf),
    /// Preprocessed assembly names an input that compiler dependency output
    /// does not report.
    #[error("preprocessed assembly uses an assembler input directive in {0}")]
    AssemblerInputDirective(PathBuf),
    /// The injected depfile could not be parsed exactly.
    #[error("could not model the compiler depfile: {0}")]
    MalformedDepfile(String),
    /// The injected depfile could not be read.
    #[error("could not read the compiler depfile {path}: {message}")]
    DepfileRead {
        /// Depfile that could not be read.
        path: PathBuf,
        /// Underlying error text.
        message: String,
    },
    /// The action exceeds an input, byte, or manifest bound.
    #[error("compilation reads more inputs than the cache adapter models")]
    TooManyInputs,
    /// An absolute path lies outside every mapped and system root.
    #[error("path is outside every modeled root: {0}")]
    UnmappedAbsolutePath(PathBuf),
    /// A path cannot be represented in the canonical UTF-8 key.
    #[error("path is not valid UTF-8: {0}")]
    NonUtf8Path(PathBuf),
    /// The compiler working directory is not absolute.
    #[error("compiler working directory is not absolute: {0}")]
    RelativeWorkingDirectory(PathBuf),
    /// A configured path mapping root is not absolute.
    #[error("path mapping root is not absolute: {0}")]
    RelativePathMapping(PathBuf),
    /// A configured placeholder is empty, duplicated, or not a bare name.
    #[error("invalid path mapping placeholder: {0}")]
    InvalidPathPlaceholder(String),
    /// A required input never appeared among the discovered inputs.
    #[error("required input is missing from the discovered inputs: {0}")]
    MissingRequiredInput(String),
    /// An input digest is malformed.
    #[error("invalid digest for input: {0}")]
    InvalidInputDigest(String),
    /// One normalized path carries two different digests.
    #[error("conflicting digests for input: {0}")]
    ConflictingInput(String),
    /// An input could not be read.
    #[error("could not read input {path}: {message}")]
    InputRead {
        /// Input that could not be read.
        path: PathBuf,
        /// Underlying error text.
        message: String,
    },
    /// An input changed between discovery and publication.
    #[error("input changed during the compilation: {0}")]
    InputChanged(PathBuf),
    /// An input was written while the compiler ran.
    #[error("input was modified during the compilation: {0}")]
    InputModifiedDuringCompilation(PathBuf),
    /// Discovery and the action disagree about the working directory.
    #[error("discovered inputs use a different working directory")]
    DiscoveryWorkingDirectory,
    /// A prediction uses a schema this adapter version does not model.
    #[error("action prediction is not modeled by this adapter version")]
    UnsupportedPrediction,
    /// A predicted input name cannot be resolved back to a host path.
    #[error("invalid predicted input: {0}")]
    InvalidPredictedInput(String),
    /// Canonical serialization failed.
    #[error("could not serialize the action descriptor: {0}")]
    Serialization(String),
}

impl From<PathNormalizationError> for CcBypassReason {
    fn from(reason: PathNormalizationError) -> Self {
        match reason {
            PathNormalizationError::UnmappedAbsolutePath(path) => Self::UnmappedAbsolutePath(path),
            PathNormalizationError::NonUtf8Path(path) => Self::NonUtf8Path(path),
        }
    }
}

/// Source language a driver invocation compiles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CcLanguage {
    /// C, driven through `CC`.
    C,
    /// C++, driven through `CXX`.
    Cxx,
}

impl CcLanguage {
    /// Shim file stem that selects this language.
    ///
    /// Neither stem ends in `-cc` or `-gcc`. Build tooling reads those
    /// suffixes off `CC` as a cross-compiler prefix -- the `autotools` crate
    /// strips one and hands the remainder to `configure` as `--host` -- so a
    /// shim named `mbx-cc` made an absolute shim path look like a machine
    /// triple.
    pub fn shim_stem(self) -> &'static str {
        match self {
            Self::C => "mbx-c",
            Self::Cxx => "mbx-cxx",
        }
    }

    /// Default driver name to fall back to when no real compiler is pinned.
    pub fn default_driver(self) -> &'static str {
        if cfg!(windows) {
            return "cl.exe";
        }
        match self {
            Self::C => "cc",
            Self::Cxx => "c++",
        }
    }
}

/// Compiler family, which decides how the identity is assembled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CcCompilerFamily {
    /// GCC, which compiles objects through an external assembler.
    Gcc,
    /// Upstream LLVM clang.
    Clang,
    /// Apple's clang distribution.
    AppleClang,
    /// Microsoft's `cl.exe` driver.
    #[cfg(windows)]
    Msvc,
}

/// Source preprocessed to ask `cl.exe` who it is.
///
/// The macros survive unexpanded in any compiler that does not define them, so
/// the answer does not depend on banner wording or the console locale.
pub const MSVC_IDENTITY_SOURCE: &str = "mbx_msc_ver _MSC_VER\nmbx_msc_full_ver _MSC_FULL_VER\nmbx_msc_build _MSC_BUILD\nmbx_clang __clang__\nmbx_x64 _M_X64\nmbx_x86 _M_IX86\nmbx_arm64 _M_ARM64\nmbx_arm64ec _M_ARM64EC\nmbx_arm _M_ARM\n";

/// Leading text of an identity built by [`msvc_identity`].
pub const MSVC_IDENTITY_PREFIX: &str = "mbx-msvc-identity";

/// Build the identity text for `cl.exe` from its preprocessed
/// [`MSVC_IDENTITY_SOURCE`], or `None` when the compiler is not MSVC.
///
/// `clang-cl` defines `_MSC_VER` too, so `__clang__` rejects it: it is a
/// different compiler with its own version line.
pub fn msvc_identity(preprocessed: &str) -> Option<String> {
    let value = |name: &str| {
        preprocessed.lines().find_map(|line| {
            let (key, rest) = line.trim().split_once(' ')?;
            (key == name).then(|| rest.trim().to_string())
        })
    };
    let defined = |text: &str, macro_name: &str| {
        !text.is_empty() && text != macro_name && text.bytes().all(|b| b.is_ascii_digit())
    };
    let ver = value("mbx_msc_ver").filter(|v| defined(v, "_MSC_VER"))?;
    let full = value("mbx_msc_full_ver").filter(|v| defined(v, "_MSC_FULL_VER"))?;
    let build = value("mbx_msc_build")
        .filter(|v| defined(v, "_MSC_BUILD"))
        .unwrap_or_default();
    if value("mbx_clang").is_some_and(|v| v != "__clang__") {
        return None;
    }
    // The version macros match across the x86, x64 and ARM64 toolset binaries,
    // so the architecture the compiler targets has to be keyed separately.
    let architecture = [
        ("mbx_x64", "_M_X64"),
        ("mbx_x86", "_M_IX86"),
        ("mbx_arm64", "_M_ARM64"),
        ("mbx_arm64ec", "_M_ARM64EC"),
        ("mbx_arm", "_M_ARM"),
    ]
    .iter()
    .filter(|(key, macro_name)| value(key).is_some_and(|v| v != *macro_name))
    .map(|(_, macro_name)| *macro_name)
    .collect::<Vec<_>>()
    .join(",");
    Some(format!(
        "{MSVC_IDENTITY_PREFIX} _MSC_VER={ver} _MSC_FULL_VER={full} _MSC_BUILD={build} ARCH={architecture}\n"
    ))
}

impl CcCompilerFamily {
    /// Stable name recorded in the action key.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Gcc => "gcc",
            Self::Clang => "clang",
            Self::AppleClang => "apple-clang",
            #[cfg(windows)]
            Self::Msvc => "msvc",
        }
    }

    /// Whether objects are produced through a separate assembler binary whose
    /// version therefore belongs in the identity.
    pub fn uses_external_assembler(self) -> bool {
        matches!(self, Self::Gcc)
    }

    /// Whether this is Microsoft's `cl.exe` driver.
    pub fn is_msvc(self) -> bool {
        #[cfg(windows)]
        {
            matches!(self, Self::Msvc)
        }
        #[cfg(not(windows))]
        {
            false
        }
    }

    /// Classify a driver from its verbose probe output.
    pub fn classify(probe: &str) -> Result<Self, CcBypassReason> {
        #[cfg(windows)]
        if probe.starts_with(MSVC_IDENTITY_PREFIX) {
            return Ok(Self::Msvc);
        }
        if probe.contains("Apple clang version") {
            Ok(Self::AppleClang)
        } else if probe.contains("clang version") {
            Ok(Self::Clang)
        } else if probe.contains("gcc version") {
            Ok(Self::Gcc)
        } else {
            Err(CcBypassReason::UnsupportedCompilerDriver(
                probe.lines().next().unwrap_or_default().into(),
            ))
        }
    }
}

/// Compiler properties that distinguish incompatible objects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CcCompilerIdentity {
    /// Driver family.
    pub family: CcCompilerFamily,
    /// Complete verbose probe output, verbatim.
    pub version_text: String,
    /// Target triple the driver reports.
    pub target: String,
    /// Resolved assembler and its version, for families that use one.
    ///
    /// GCC assembles through binutils, whose version changes object bytes
    /// without changing anything `gcc -v` prints. Clang assembles internally,
    /// so this is empty there.
    pub assembler: String,
    /// Where the driver lives, when that is the Nix store.
    pub nix: Option<NixStoreDriver>,
}

/// A driver in the Nix store, identified by its store path.
///
/// A Nix cc-wrapper answers `-v` for the compiler it wraps, so two wrappers
/// around one compiler print the same banner while adding different flags of
/// their own: libc and C++ library include directories, frame-pointer and
/// hardening defaults, a `-target`. Each of those is part of the wrapper's
/// store path, which names everything that built it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NixStoreDriver {
    /// The executable's canonical path, beneath [`NIX_STORE`].
    pub path: PathBuf,
    /// Whether it is a nixpkgs cc-wrapper, which rewrites its command line
    /// from the environment (see [`NIX_CC_WRAPPER_ENVIRONMENT`]).
    pub cc_wrapper: bool,
}

/// One file input paired with the digest used in the action key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CcActionInput {
    /// Absolute host path used to read and verify the input, or an
    /// include-manifest pseudo-path.
    pub path: PathBuf,
    /// Digest of the input contents, or of the directory's name manifest.
    pub digest: CacheDigest,
}

/// External information needed to construct a canonical cc action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CcActionContext {
    /// Identity of the compiler that produces the object.
    pub compiler: CcCompilerIdentity,
    /// Absolute directory in which the compiler runs.
    pub working_dir: PathBuf,
    /// Host roots replaced with stable placeholders in the key.
    pub path_mappings: Vec<PathMapping>,
    /// Environment inputs and their observed values.
    pub environment: BTreeMap<String, Option<String>>,
    /// Complete set of direct and discovered file inputs.
    pub inputs: Vec<CcActionInput>,
}

/// Canonical action descriptor and its content digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CcAction {
    /// Digest of `bytes`, used as the action-cache key.
    pub digest: CacheDigest,
    /// Canonical serialized action descriptor.
    pub bytes: Vec<u8>,
}

#[derive(Debug, Serialize)]
struct CcCompilerDescriptor {
    assembler: String,
    family: String,
    /// Absent for a driver outside the Nix store, so its keys are what they
    /// were before store drivers were told apart.
    #[serde(skip_serializing_if = "Option::is_none")]
    nix_store_path: Option<PathBuf>,
    target: String,
    version_text: String,
}

#[derive(Debug, Serialize)]
struct CcInputDescriptor {
    digest: CacheDigest,
    path: String,
}

#[derive(Debug, Serialize)]
struct CcActionDescriptor {
    #[serde(skip_serializing_if = "Option::is_none")]
    path_binding: Option<CcPathBinding>,
    version: u8,
    kind: &'static str,
    adapter_version: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    assembly_input_model: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    preprocessing: Option<PreprocessingContext>,
    compiler: CcCompilerDescriptor,
    arguments: Vec<String>,
    environment: BTreeMap<String, Option<String>>,
    inputs: Vec<CcInputDescriptor>,
}

#[derive(Debug, Serialize)]
struct CcInvocationDescriptor {
    version: u8,
    kind: &'static str,
    adapter_version: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    assembly_input_model: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    preprocessing: Option<PreprocessingContext>,
    compiler: CcCompilerDescriptor,
    arguments: Vec<String>,
    required_inputs: Vec<String>,
}

/// Preprocessor text embeds literal paths in line markers and macros. Keep
/// its invocation tied to those spellings instead of rewriting user output.
#[derive(Debug, Serialize)]
struct PreprocessingContext {
    working_dir: PathBuf,
    arguments: Vec<String>,
    roots: BTreeMap<String, PathBuf>,
}

/// Literal path spellings retained by an object, unlike portable debug paths.
#[derive(Debug, Serialize)]
struct CcPathBinding {
    working_dir: PathBuf,
    arguments: Vec<Argument>,
    roots: BTreeMap<String, PathBuf>,
}

/// Normalized input names from the last successful execution of one modeled
/// compile.
///
/// From [`COMPACT_PREDICTION_VERSION`] on, the serialized form front-codes
/// `inputs` ([`compact_prediction_inputs`]): a C++ compilation names hundreds
/// of system headers under one long SDK directory, and spelled out in full a
/// build of a few hundred of them outgrows the task manifest that carries its
/// predictions to the next checkout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CcInputPrediction {
    /// The output retained literal paths and requires a path-specific action key.
    pub path_specific: bool,
    /// Prediction schema version.
    pub version: u8,
    /// Normalized input paths, including include-manifest entries.
    pub inputs: Vec<String>,
    /// Names of environment variables that entered the key.
    pub environment: Vec<String>,
    /// Compiler wall time from the successful invocation that produced this
    /// prediction. Zero means no timing hint was recorded.
    pub compiler_duration_ns: u64,
    /// Source file name associated with the timing hint.
    pub source_name: String,
}

/// The prediction schema version this adapter writes, the first whose inputs
/// are front-coded. Version 1, inputs spelled out, is still read.
pub const COMPACT_PREDICTION_VERSION: u8 = 2;

/// [`CcInputPrediction`] as serialized; `inputs` is front-coded at
/// [`COMPACT_PREDICTION_VERSION`].
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CcInputPredictionWire<'a> {
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    path_specific: bool,
    version: u8,
    inputs: Cow<'a, [String]>,
    environment: Cow<'a, [String]>,
    #[serde(default, skip_serializing_if = "is_zero")]
    compiler_duration_ns: u64,
    #[serde(default, skip_serializing_if = "str::is_empty")]
    source_name: Cow<'a, str>,
}

impl Serialize for CcInputPrediction {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        CcInputPredictionWire {
            path_specific: self.path_specific,
            version: self.version,
            inputs: if self.version == COMPACT_PREDICTION_VERSION {
                Cow::Owned(compact_prediction_inputs(&self.inputs))
            } else {
                Cow::Borrowed(&self.inputs)
            },
            environment: Cow::Borrowed(&self.environment),
            compiler_duration_ns: self.compiler_duration_ns,
            source_name: Cow::Borrowed(&self.source_name),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for CcInputPrediction {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = CcInputPredictionWire::deserialize(deserializer)?;
        let inputs = if wire.version == COMPACT_PREDICTION_VERSION {
            expand_prediction_inputs(&wire.inputs).map_err(serde::de::Error::custom)?
        } else {
            wire.inputs.into_owned()
        };
        Ok(Self {
            path_specific: wire.path_specific,
            version: wire.version,
            inputs,
            environment: wire.environment.into_owned(),
            compiler_duration_ns: wire.compiler_duration_ns,
            source_name: wire.source_name.into_owned(),
        })
    }
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

/// One parsed and admitted argument.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
enum Argument {
    /// Keyed verbatim.
    Plain(String),
    /// Keyed with its path normalized.
    Path { flag: String, path: PathBuf },
    /// A prefix rewrite: the source path normalizes, the replacement does not.
    PrefixMap {
        flag: String,
        from: PathBuf,
        to: String,
    },
    /// The source file.
    Source(PathBuf),
}

/// A parsed, admitted C or C++ compile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CcInvocation {
    arguments: Vec<Argument>,
    source: PathBuf,
    output: PathBuf,
    include_dirs: Vec<PathBuf>,
    required_inputs: Vec<PathBuf>,
    language: CcLanguage,
    preprocessed_assembly: bool,
    preprocessing_arguments: Option<Vec<String>>,
    sysroot: Option<PathBuf>,
    caller_depfile: Option<CallerDepfile>,
    /// Positions in the parsed command line of the dependency-list flags the
    /// caller passed, which the shim leaves out when it runs the driver.
    dependency_argument_indices: Vec<usize>,
}

/// A dependency list the caller asked the driver to write beside the object.
///
/// Build systems driving the compiler through a build script -- OpenSSL's
/// makefiles under `openssl-src`, CMake under many `-sys` crates -- ask for one
/// with `-MD` or `-MMD` so their own rebuild logic has something to read. The
/// flags do not change the object, so they are not key material, and the file
/// names paths on this machine, so it is never stored. The shim writes it
/// itself from the dependency list it already collects, whether the object
/// was compiled or restored.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct CallerDepfile {
    /// Where the list goes: `-MF`, or the object's path with a `.d` extension.
    pub path: PathBuf,
    /// The rule's targets, from `-MT` and `-MQ`, or the object as `-o` named it.
    pub targets: Vec<DepfileTarget>,
    /// Whether system headers are left out, as `-MMD` does.
    pub user_headers_only: bool,
    /// Whether every header also gets an empty rule of its own, as `-MP` does.
    pub phony_targets: bool,
}

/// One target of a caller's dependency rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DepfileTarget {
    /// The target text as the caller gave it.
    pub name: String,
    /// Whether characters special to make are quoted on output, as `-MQ`
    /// asks; `-MT` writes the name literally.
    pub quoted: bool,
}

impl CcInvocation {
    /// The object's path made absolute against the compiler's working
    /// directory, for reading and storing it. `-o` is often relative --
    /// OpenSSL's makefiles name every object that way -- and a path handed to
    /// the cache agent has to mean the same file from the agent's own
    /// directory. Key material still normalizes the spelling as given.
    pub fn output_in(&self, working_dir: &Path) -> PathBuf {
        if self.output.is_absolute() {
            normalize_components(&self.output)
        } else {
            normalize_components(&working_dir.join(&self.output))
        }
    }

    /// The dependency list the caller asked for, if any.
    pub fn caller_depfile(&self) -> Option<&CallerDepfile> {
        self.caller_depfile.as_ref()
    }

    /// The command line to run the driver with: `arguments` as parsed, less
    /// the caller's dependency-list flags. The shim asks the driver for its
    /// own list, the driver honors only one `-MF`, and the caller's list is
    /// written by the shim afterwards.
    pub fn compiler_arguments(&self, arguments: &[OsString]) -> Vec<OsString> {
        arguments
            .iter()
            .enumerate()
            .filter(|(index, _)| !self.dependency_argument_indices.contains(index))
            .map(|(_, argument)| argument.clone())
            .collect()
    }

    /// Parse a driver command line, admitting modeled single-source compiles
    /// and preprocessing to a named output.
    pub fn parse(arguments: &[OsString]) -> Result<Self, CcBypassReason> {
        Parser::new(arguments).parse()
    }

    /// Parse a command line using the syntax of `family`.
    pub fn parse_for(
        arguments: &[OsString],
        family: CcCompilerFamily,
    ) -> Result<Self, CcBypassReason> {
        if family.is_msvc() {
            MsvcParser::new(arguments).parse()
        } else {
            let mut parser = Parser::new(arguments);
            parser.family = family;
            parser.parse()
        }
    }

    /// Parse a command line using Microsoft `cl.exe` syntax.
    pub fn parse_msvc(arguments: &[OsString]) -> Result<Self, CcBypassReason> {
        MsvcParser::new(arguments).parse()
    }

    /// Source file this invocation compiles.
    pub fn source(&self) -> &Path {
        &self.source
    }

    /// Whether this invocation writes named preprocessor text.
    pub fn is_preprocessing(&self) -> bool {
        self.preprocessing_arguments.is_some()
    }

    /// Output file this invocation produces.
    pub fn output(&self) -> &Path {
        &self.output
    }

    /// Include search directories named on the command line, in order.
    pub fn include_dirs(&self) -> &[PathBuf] {
        &self.include_dirs
    }

    /// Files that must appear among the discovered inputs.
    pub fn required_inputs(&self) -> &[PathBuf] {
        &self.required_inputs
    }

    /// Language the driver compiles.
    pub fn language(&self) -> CcLanguage {
        self.language
    }

    /// Reject assembler-time file directives that the preprocessor's
    /// dependency list cannot name.
    ///
    /// This is a no-op for C and C++ inputs. For preprocessed assembly, every
    /// file reported by dependency discovery is scanned conservatively before
    /// an object can be published.
    pub fn validate_discovered_inputs<'a>(
        &self,
        paths: impl IntoIterator<Item = &'a Path>,
    ) -> Result<(), CcBypassReason> {
        if !self.preprocessed_assembly {
            return Ok(());
        }
        for path in paths {
            if depfile::contains_assembler_input_directive(path)? {
                return Err(CcBypassReason::AssemblerInputDirective(path.to_path_buf()));
            }
        }
        Ok(())
    }

    /// Sysroot named on the command line, if any.
    pub fn sysroot(&self) -> Option<&Path> {
        self.sysroot.as_deref()
    }

    /// Short label used for timing statistics.
    pub fn source_name(&self) -> String {
        self.source
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    /// Arguments to append so the driver writes a dependency list beside the
    /// object.
    ///
    /// `-MD` rather than `-MMD`: system headers are exactly the inputs most
    /// likely to change without any other key component noticing, because the
    /// compiler identity does not cover the C library or the platform SDK.
    ///
    /// A compilation that uses a precompiled header also gets
    /// `-Xclang -module-file-deps`. Clang leaves module files out of the list
    /// otherwise, and a PCH built while another was loaded makes Clang load
    /// that one too, from the path recorded inside it: with the option the
    /// list names both, so the PCH a chain loads is keyed by its bytes like
    /// any header read. Without it, rebuilding that first PCH under another
    /// definition would restore an object built against the old one.
    pub fn dependency_arguments(&self, depfile: &Path) -> Vec<OsString> {
        let mut arguments = vec!["-MD".into(), "-MF".into(), depfile.into()];
        if self.arguments.iter().any(|argument| {
            matches!(argument, Argument::Path { flag, .. }
                if flag == "-include-pch" || flag == "-Xclang -include-pch")
        }) {
            arguments.extend(["-Xclang".into(), "-module-file-deps".into()]);
        }
        arguments
    }

    /// Arguments to append so a driver from `family` writes its dependency
    /// list beside the object.
    pub fn dependency_arguments_for(
        &self,
        depfile: &Path,
        family: CcCompilerFamily,
    ) -> Vec<OsString> {
        if family.is_msvc() {
            vec!["/sourceDependencies".into(), depfile.into()]
        } else {
            self.dependency_arguments(depfile)
        }
    }

    /// Arguments to append so `cl.exe` writes `/sourceDependencies` JSON.
    pub fn msvc_dependency_arguments(&self, depfile: &Path) -> Vec<OsString> {
        vec!["/sourceDependencies".into(), depfile.into()]
    }

    /// Digest of the pre-input fingerprint, used to look up a prediction.
    pub fn invocation_digest(
        &self,
        context: &CcActionContext,
    ) -> Result<CacheDigest, CcBypassReason> {
        let builder = ActionBuilder::new(self, context.clone());
        let descriptor = builder.invocation_descriptor()?;
        let bytes = canonical_json(&descriptor)
            .map_err(|error| CcBypassReason::Serialization(error.to_string()))?;
        Ok(CacheDigest::blake3(&bytes))
    }

    /// Build the canonical action for this invocation and its discovered
    /// inputs.
    pub fn action(&self, context: CcActionContext) -> Result<CcAction, CcBypassReason> {
        self.action_with_path_binding(context, false)
    }

    /// Build an action tied to literal path spellings when the output retained them.
    /// Portable actions keep their existing keys.
    pub fn action_with_path_binding(
        &self,
        context: CcActionContext,
        path_specific: bool,
    ) -> Result<CcAction, CcBypassReason> {
        ActionBuilder::new(self, context).build(path_specific)
    }

    /// Record the normalized inputs of a successful compile so the next cold
    /// invocation can rebuild the same key before compiling.
    pub fn prediction(
        &self,
        context: &CcActionContext,
        compiler_duration_ns: u64,
    ) -> Result<CcInputPrediction, CcBypassReason> {
        let builder = ActionBuilder::new(self, context.clone());
        let mut inputs = context
            .inputs
            .iter()
            .map(|input| builder.normalize_input_path(&input.path))
            .collect::<Result<Vec<_>, _>>()?;
        inputs.sort();
        inputs.dedup();
        Ok(CcInputPrediction {
            path_specific: false,
            version: COMPACT_PREDICTION_VERSION,
            inputs,
            environment: context.environment.keys().cloned().collect(),
            compiler_duration_ns,
            source_name: self.source_name(),
        })
    }
}

impl CcInputPrediction {
    /// Rehash the predicted paths and recompute include manifests. The caller
    /// still recomputes the full action digest, so changed inputs are misses.
    pub fn discover(
        &self,
        working_dir: &Path,
        path_mappings: &[PathMapping],
        digests: &dyn FileDigestCache,
    ) -> Result<CcDiscoveredInputs, CcBypassReason> {
        if !matches!(self.version, 1..=COMPACT_PREDICTION_VERSION) {
            return Err(CcBypassReason::UnsupportedPrediction);
        }
        if self.inputs.len() > MAX_PREDICTED_INPUTS || self.environment.len() > 4 * 1024 {
            return Err(CcBypassReason::UnsupportedPrediction);
        }
        let mappings = PathMapping::ordered(path_mappings);
        let mut files = BTreeSet::new();
        let mut directories = BTreeSet::new();
        for entry in &self.inputs {
            match entry.strip_prefix(INCLUDE_MANIFEST_PREFIX) {
                Some(directory) => {
                    directories.insert(denormalize_path(directory, &mappings)?);
                }
                None => {
                    files.insert(denormalize_path(entry, &mappings)?);
                }
            }
        }
        CcDiscoveredInputs::collect(working_dir, files, directories, digests)
    }
}

/// Resolve a normalized key path back to a host path.
///
/// Placeholder entries expand through their mapping; a verbatim entry is
/// accepted only when it still lies beneath an admitted system root, so a
/// prediction cannot name an arbitrary absolute path.
fn denormalize_path(value: &str, mappings: &[PathMapping]) -> Result<PathBuf, CcBypassReason> {
    for mapping in mappings {
        let prefix = format!("${{{}}}", mapping.placeholder);
        let suffix = if value == prefix {
            ""
        } else if let Some(suffix) = value.strip_prefix(&format!("{prefix}/")) {
            suffix
        } else {
            continue;
        };
        if !mapping.root.is_absolute() || !safe_suffix(suffix) {
            return Err(CcBypassReason::InvalidPredictedInput(value.into()));
        }
        let mut path = normalize_components(&mapping.root);
        path.extend(suffix.split('/').filter(|component| !component.is_empty()));
        return Ok(path);
    }
    // A verbatim entry names a machine path rather than a placeholder. It is
    // admitted only beneath a system root, and only spelled literally: a
    // traversal component would let a prediction reach outside that root.
    let path = PathBuf::from(value);
    if path.is_absolute() && is_system_path(&path) && normalize_components(&path) == path {
        return Ok(path);
    }
    Err(CcBypassReason::InvalidPredictedInput(value.into()))
}

fn safe_suffix(suffix: &str) -> bool {
    suffix.is_empty()
        || !suffix.split('/').any(|component| {
            component.is_empty() || matches!(component, "." | "..") || component.contains('\\')
        })
}

/// Whether a path lies beneath a root whose location is a machine property.
pub fn is_system_path(path: &Path) -> bool {
    SYSTEM_ROOTS
        .iter()
        .any(|root| path.starts_with(Path::new(root)))
        || developer_roots().iter().any(|root| path.starts_with(root))
}

// Xcode may be renamed or installed outside /Applications. Match the active
// developer directory rather than assuming the application bundle's name.
// Keep both spellings because compiler depfiles may resolve SDK symlinks.
#[cfg(target_os = "macos")]
fn developer_roots() -> &'static [PathBuf] {
    static ROOTS: std::sync::OnceLock<Vec<PathBuf>> = std::sync::OnceLock::new();
    ROOTS.get_or_init(|| {
        let selected = std::env::var_os("DEVELOPER_DIR")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                let output = std::process::Command::new("/usr/bin/xcode-select")
                    .arg("--print-path")
                    .output()
                    .ok()?;
                if !output.status.success() {
                    return None;
                }
                Some(PathBuf::from(
                    std::str::from_utf8(&output.stdout).ok()?.trim(),
                ))
            });
        selected.map(developer_directory_roots).unwrap_or_default()
    })
}

#[cfg(not(target_os = "macos"))]
fn developer_roots() -> &'static [PathBuf] {
    &[]
}

#[cfg(any(target_os = "macos", all(test, unix)))]
fn developer_directory_roots(mut root: PathBuf) -> Vec<PathBuf> {
    if !root.is_absolute() || root == Path::new("/") {
        return Vec::new();
    }
    // xcrun also accepts DEVELOPER_DIR pointing at the application bundle.
    if root.extension().is_some_and(|extension| extension == "app") {
        root = root.join("Contents/Developer");
    }
    root = normalize_components(&root);
    if root == Path::new("/") {
        return Vec::new();
    }
    let mut roots = vec![root.clone()];
    if let Ok(canonical) = std::fs::canonicalize(&root)
        && canonical != root
    {
        roots.push(canonical);
    }
    roots
}

fn normalize_components(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            component => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

/// The compiler's process environment, by name.
type Variables = BTreeMap<OsString, OsString>;

impl CcInvocation {
    /// Read the environment this compile depends on from the variables the
    /// compiler runs with.
    ///
    /// Variables that change the compile in a way the argv model cannot see
    /// are refused. The modeled ones enter the key with their values; one whose
    /// value is not UTF-8 cannot be keyed and is refused too, rather than read
    /// as unset.
    pub fn environment_inputs<I>(
        &self,
        variables: I,
        compiler: &CcCompilerIdentity,
    ) -> Result<BTreeMap<String, Option<String>>, CcBypassReason>
    where
        I: IntoIterator<Item = (OsString, OsString)>,
    {
        let variables: Variables = variables.into_iter().collect();
        let value = |name: &str| variable(&variables, name);
        for name in BYPASS_ENVIRONMENT {
            if variables.contains_key(OsStr::new(name)) {
                return Err(CcBypassReason::UnsupportedEnvironment((*name).into()));
            }
        }
        let mut environment = BTreeMap::new();
        for name in KEYED_ENVIRONMENT {
            // An explicit `-isysroot` on the command line already pins the SDK,
            // and it is what the driver honors, so the variable stops being an
            // input.
            if *name == "SDKROOT" && self.sysroot.is_some() {
                continue;
            }
            environment.insert((*name).to_string(), value(name)?);
        }
        if compiler.family.is_msvc() {
            // INCLUDE changes header resolution without appearing in argv. The
            // toolset and SDK versions make the otherwise machine-local paths
            // meaningful when action records move between hosts.
            for name in [
                "INCLUDE",
                "VCToolsVersion",
                "WindowsSDKVersion",
                "UCRTVersion",
            ] {
                environment.insert(name.into(), value(name)?);
            }
            for name in ["CL", "_CL_"] {
                if variables.contains_key(OsStr::new(name)) {
                    return Err(CcBypassReason::UnsupportedEnvironment(name.into()));
                }
            }
        }
        for name in variables.keys().filter_map(|name| name.to_str()) {
            if in_family(NIX_CC_WRAPPER_ENVIRONMENT, name) {
                environment.insert(name.to_owned(), value(name)?);
            }
        }
        if compiler
            .nix
            .as_ref()
            .is_some_and(|driver| driver.cc_wrapper)
        {
            nix_wrapper_flags(&variables, compiler.family)?;
            self.nix_wrapper_purity(&variables)?;
        }
        Ok(environment)
    }

    /// Refuse a compile whose search paths a Nix cc-wrapper would drop.
    ///
    /// With `NIX_ENFORCE_PURITY=1` and `NIX_STORE` set, nixpkgs' wrapper
    /// removes every `-I` and `-isystem` whose absolute path, other than
    /// `/dev/null`, starts with none of `NIX_STORE`, `NIX_BUILD_TOP`, `/tmp`,
    /// `TMP`, `TMPDIR`, `TEMP`, and `TEMPDIR` (the last four `/tmp` when unset
    /// or empty), compared as text: `badPath` in its `utils.bash`. An unset
    /// `NIX_BUILD_TOP` aborts the wrapper instead. Either way the compiler does
    /// not search what the key says it did, and the directories that decide it
    /// differ between builds that share a key, so the outcome is checked here
    /// rather than keyed.
    ///
    /// On Darwin the wrapper first weighs a path at its spelling inside the SDK
    /// it selects from the `DEVELOPER_DIR` variables, when that exists. An SDK
    /// the same test accepts accepts every path inside it, so refusing an
    /// impure `DEVELOPER_DIR` value leaves the plain test as the whole answer.
    fn nix_wrapper_purity(&self, variables: &Variables) -> Result<(), CcBypassReason> {
        let value = |name: &str| variable(variables, name);
        if value("NIX_ENFORCE_PURITY")?.as_deref() != Some("1") {
            return Ok(());
        }
        let Some(store) = value("NIX_STORE")?.filter(|store| !store.is_empty()) else {
            return Ok(());
        };
        let searched = self
            .arguments
            .iter()
            .filter_map(|argument| match argument {
                Argument::Path { flag, path } if matches!(flag.as_str(), "-I" | "-isystem") => {
                    Some(path)
                }
                _ => None,
            })
            .filter(|path| path.is_absolute())
            .collect::<Vec<_>>();
        if searched.is_empty() {
            return Ok(());
        }
        let build_top = value("NIX_BUILD_TOP")?;
        let mut pure = vec![store, "/tmp".to_owned()];
        for name in ["TMP", "TMPDIR", "TEMP", "TEMPDIR"] {
            pure.push(
                value(name)?
                    .filter(|root| !root.is_empty())
                    .unwrap_or_else(|| "/tmp".into()),
            );
        }
        pure.extend(build_top.clone().filter(|top| !top.is_empty()));
        // A relative path is one the wrapper assumes sits in the build
        // directory, and keeps.
        let impure = |path: &str| {
            path.starts_with('/')
                && path != "/dev/null"
                && (build_top.is_none() || !pure.iter().any(|root| path.starts_with(root.as_str())))
        };
        for name in variables.keys().filter_map(|name| name.to_str()) {
            if in_family(&["DEVELOPER_DIR"], name)
                && let Some(root) = value(name)?
                && impure(&root)
            {
                return Err(CcBypassReason::NixImpurePath(root.into()));
            }
        }
        match searched
            .into_iter()
            .find(|path| path.to_str().is_none_or(impure))
        {
            Some(path) => Err(CcBypassReason::NixImpurePath(path.clone())),
            None => Ok(()),
        }
    }
}

/// The value of `name`, refused when it cannot be keyed as text.
fn variable(variables: &Variables, name: &str) -> Result<Option<String>, CcBypassReason> {
    variables
        .get(OsStr::new(name))
        .map(|value| {
            value
                .to_str()
                .map(ToOwned::to_owned)
                .ok_or_else(|| CcBypassReason::UnsupportedEnvironment(name.into()))
        })
        .transpose()
}

/// Whether `name` is one of `families` or extends one with `_`.
fn in_family(families: &[&str], name: &str) -> bool {
    families.iter().any(|family| {
        name.strip_prefix(family)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('_'))
    })
}

/// Run the words a Nix cc-wrapper adds to the compiler's command line from
/// the environment through the model argv goes through.
///
/// They are the compiler's arguments as much as argv's are, so each one meets
/// the same model: what bypasses on argv bypasses here (`-march=native`,
/// `-gsplit-dwarf`, `-fplugin=`, `-Xclang`, ...). Their text is already keyed
/// with the variable. What would change how the compile itself is modeled is
/// refused: an output, a language, a mode, a sysroot, a source, or the
/// caller's own dependency output, which a hit would not restore.
///
/// The wrapper expands each variable unquoted, so it splits on spaces, tabs,
/// and newlines and globs every word under `nullglob`: a word naming files the
/// key cannot see, or none at all, so one carrying a glob character is
/// refused. A search directory a word adds is not on the command line either,
/// so the include manifest cannot watch it for a header that would shadow one
/// the compilation read (see [`manifest_snapshot`]). In the Nix store it needs
/// no watching -- store paths do not change -- and system roots are exempt
/// anyway; anywhere else the variable is refused.
fn nix_wrapper_flags(
    variables: &Variables,
    family: CcCompilerFamily,
) -> Result<(), CcBypassReason> {
    for name in variables.keys().filter_map(|name| name.to_str()) {
        if !in_family(NIX_CC_WRAPPER_FLAGS, name) {
            continue;
        }
        let Some(value) = variable(variables, name)? else {
            continue;
        };
        let refused = || CcBypassReason::UnsupportedEnvironment(name.into());
        let words = value
            .split([' ', '\t', '\n'])
            .filter(|word| !word.is_empty())
            .map(OsString::from)
            .collect::<Vec<_>>();
        if words.iter().any(|word| {
            word.to_str()
                .is_some_and(|word| word.contains(['*', '?', '[']))
        }) {
            return Err(refused());
        }
        let mut parser = Parser::new(&words);
        parser.family = family;
        while parser.index < words.len() {
            let word = parser.current()?.to_owned();
            parser.index += 1;
            if !word.starts_with('-') || word == "-" {
                return Err(refused());
            }
            parser.parse_flag(&word)?;
        }
        if !parser.dependency.indices.is_empty() {
            return Err(CcBypassReason::CallerDependencyFlags(name.into()));
        }
        if parser.source.is_some()
            || parser.output.is_some()
            || parser.explicit_language.is_some()
            || parser.preprocessing
            || parser.sysroot.is_some()
            || parser.include_dirs.iter().any(|directory| {
                !directory.is_absolute()
                    || normalize_components(directory) != *directory
                    || !is_system_path(directory)
            })
        {
            return Err(refused());
        }
    }
    Ok(())
}

struct ActionBuilder<'a> {
    invocation: &'a CcInvocation,
    context: CcActionContext,
    mappings: Vec<PathMapping>,
    /// Reuse parent resolution across the inputs of this action only.
    aliases: PathAliases,
}

impl<'a> ActionBuilder<'a> {
    fn new(invocation: &'a CcInvocation, mut context: CcActionContext) -> Self {
        context.path_mappings = PathMapping::ordered(&context.path_mappings);
        let mappings = resolve_path_mappings(&context.path_mappings);
        Self {
            invocation,
            context,
            mappings,
            aliases: PathAliases::default(),
        }
    }

    fn build(self, path_specific: bool) -> Result<CcAction, CcBypassReason> {
        self.validate_mappings()?;
        let invocation = self.invocation_descriptor()?;

        let mut inputs = BTreeMap::<String, CacheDigest>::new();
        for input in &self.context.inputs {
            input.digest.validate().map_err(|_| {
                CcBypassReason::InvalidInputDigest(input.path.display().to_string())
            })?;
            let path = self.normalize_input_path(&input.path)?;
            if inputs
                .insert(path.clone(), input.digest.clone())
                .is_some_and(|existing| existing != input.digest)
            {
                return Err(CcBypassReason::ConflictingInput(path));
            }
        }
        let required = self
            .invocation
            .required_inputs
            .iter()
            .map(|path| self.normalize_path(path))
            .collect::<Result<BTreeSet<_>, _>>()?;
        if let Some(missing) = required.iter().find(|path| !inputs.contains_key(*path)) {
            return Err(CcBypassReason::MissingRequiredInput(missing.clone()));
        }
        let inputs = inputs
            .into_iter()
            .map(|(path, digest)| CcInputDescriptor { path, digest })
            .collect();
        let descriptor = CcActionDescriptor {
            path_binding: path_specific.then(|| CcPathBinding {
                working_dir: self.context.working_dir.clone(),
                arguments: self.invocation.arguments.clone(),
                roots: self
                    .context
                    .path_mappings
                    .iter()
                    .map(|mapping| (mapping.placeholder.clone(), mapping.root.clone()))
                    .collect(),
            }),
            version: ACTION_SCHEMA_VERSION,
            kind: "cc",
            adapter_version: ADAPTER_VERSION,
            assembly_input_model: invocation.assembly_input_model,
            preprocessing: invocation.preprocessing,
            compiler: invocation.compiler,
            arguments: invocation.arguments,
            environment: self.context.environment.clone(),
            inputs,
        };
        let bytes = canonical_json(&descriptor)
            .map_err(|error| CcBypassReason::Serialization(error.to_string()))?;
        let digest = CacheDigest::blake3(&bytes);
        Ok(CcAction { digest, bytes })
    }

    fn invocation_descriptor(&self) -> Result<CcInvocationDescriptor, CcBypassReason> {
        self.validate_mappings()?;
        let arguments = self
            .invocation
            .arguments
            .iter()
            .map(|argument| self.normalize_argument(argument))
            .collect::<Result<Vec<_>, _>>()?;
        let required_inputs = self
            .invocation
            .required_inputs
            .iter()
            .map(|path| self.normalize_path(path))
            .collect::<Result<BTreeSet<_>, _>>()?
            .into_iter()
            .collect();
        Ok(CcInvocationDescriptor {
            version: ACTION_SCHEMA_VERSION,
            kind: "cc",
            adapter_version: ADAPTER_VERSION,
            // Version the assembly-only safety model independently so adding
            // support does not invalidate every existing C and C++ entry.
            assembly_input_model: self.invocation.preprocessed_assembly.then_some(1),
            preprocessing: self
                .invocation
                .preprocessing_arguments
                .as_ref()
                .map(|arguments| PreprocessingContext {
                    working_dir: self.context.working_dir.clone(),
                    arguments: arguments.clone(),
                    roots: self
                        .mappings
                        .iter()
                        .map(|mapping| (mapping.placeholder.clone(), mapping.root.clone()))
                        .collect(),
                }),
            compiler: self.compiler_descriptor(),
            arguments,
            required_inputs,
        })
    }

    fn compiler_descriptor(&self) -> CcCompilerDescriptor {
        CcCompilerDescriptor {
            assembler: self.context.compiler.assembler.clone(),
            family: self.context.compiler.family.as_str().into(),
            nix_store_path: self
                .context
                .compiler
                .nix
                .as_ref()
                .map(|driver| driver.path.clone()),
            target: self.context.compiler.target.clone(),
            version_text: self.context.compiler.version_text.clone(),
        }
    }

    fn validate_mappings(&self) -> Result<(), CcBypassReason> {
        if !self.context.working_dir.is_absolute() {
            return Err(CcBypassReason::RelativeWorkingDirectory(
                self.context.working_dir.clone(),
            ));
        }
        let mut roots = BTreeSet::new();
        let mut placeholders = BTreeSet::new();
        for mapping in &self.context.path_mappings {
            if !mapping.root.is_absolute() {
                return Err(CcBypassReason::RelativePathMapping(mapping.root.clone()));
            }
            if mapping.placeholder.is_empty()
                || !mapping
                    .placeholder
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
                || !roots.insert(normalize_components(&mapping.root))
                || !placeholders.insert(&mapping.placeholder)
            {
                return Err(CcBypassReason::InvalidPathPlaceholder(
                    mapping.placeholder.clone(),
                ));
            }
        }
        Ok(())
    }

    fn normalize_argument(&self, argument: &Argument) -> Result<String, CcBypassReason> {
        match argument {
            Argument::Plain(value) => Ok(value.clone()),
            Argument::Path { flag, path } => Ok(format!("{flag}={}", self.normalize_path(path)?)),
            Argument::PrefixMap { flag, from, to } => {
                Ok(format!("{flag}={}={to}", self.normalize_path(from)?))
            }
            Argument::Source(path) => Ok(self.normalize_path(path)?),
        }
    }

    /// Normalize a path that names a compilation input or search root.
    ///
    /// A path beneath a mapped root becomes a placeholder so equivalent
    /// checkouts agree. A path beneath a system root stays verbatim: its
    /// location is a property of the machine, and its contents are digested
    /// like any other input.
    fn normalize_path(&self, path: &Path) -> Result<String, CcBypassReason> {
        match normalize_resolved_mapped_path_with(
            &self.aliases,
            path,
            &self.context.working_dir,
            &self.mappings,
        ) {
            Ok(normalized) => Ok(normalized),
            Err(reason) => {
                let absolute = absolute_path(path, &self.context.working_dir);
                if is_system_path(&absolute) {
                    return absolute
                        .to_str()
                        .map(ToOwned::to_owned)
                        .ok_or_else(|| CcBypassReason::NonUtf8Path(absolute.clone()));
                }
                Err(reason.into())
            }
        }
    }

    fn normalize_input_path(&self, path: &Path) -> Result<String, CcBypassReason> {
        match path.to_str().and_then(|path| {
            path.strip_prefix(INCLUDE_MANIFEST_PREFIX)
                .map(ToOwned::to_owned)
        }) {
            Some(directory) => Ok(format!(
                "{INCLUDE_MANIFEST_PREFIX}{}",
                self.normalize_path(Path::new(&directory))?
            )),
            None => self.normalize_path(path),
        }
    }
}

fn absolute_path(path: &Path, working_dir: &Path) -> PathBuf {
    if path.is_absolute() {
        normalize_components(path)
    } else {
        normalize_components(&working_dir.join(path))
    }
}

struct Parser<'a> {
    family: CcCompilerFamily,
    arguments: &'a [OsString],
    index: usize,
    parsed: Vec<Argument>,
    source: Option<PathBuf>,
    output: Option<PathBuf>,
    include_dirs: Vec<PathBuf>,
    required_inputs: Vec<PathBuf>,
    sysroot: Option<PathBuf>,
    explicit_language: Option<CcLanguage>,
    preprocessed_assembly: bool,
    compiling: bool,
    preprocessing: bool,
    dependency: DependencyRequest,
}

/// What the caller's `-M` flags asked for, gathered while parsing.
#[derive(Debug, Default)]
struct DependencyRequest {
    /// `Some(false)` for `-MD`, `Some(true)` for `-MMD`.
    user_headers_only: Option<bool>,
    file: Option<PathBuf>,
    targets: Vec<DepfileTarget>,
    phony_targets: bool,
    /// The last `-MF`, `-MT`, `-MQ`, or `-MP` seen, for the bypass a request
    /// without `-MD` or `-MMD` reports.
    modifier: Option<String>,
    indices: Vec<usize>,
}

impl<'a> Parser<'a> {
    fn new(arguments: &'a [OsString]) -> Self {
        Self {
            family: CcCompilerFamily::Gcc,
            arguments,
            index: 0,
            parsed: Vec::new(),
            source: None,
            output: None,
            include_dirs: Vec::new(),
            required_inputs: Vec::new(),
            sysroot: None,
            explicit_language: None,
            preprocessed_assembly: false,
            compiling: false,
            preprocessing: false,
            dependency: DependencyRequest::default(),
        }
    }

    fn parse(mut self) -> Result<CcInvocation, CcBypassReason> {
        while self.index < self.arguments.len() {
            let value = self.current()?.to_string();
            self.index += 1;
            if value == "-" {
                return Err(CcBypassReason::StandardInput);
            }
            if let Some(argfile) = value.strip_prefix('@') {
                return Err(CcBypassReason::ResponseFile(argfile.into()));
            }
            if value.starts_with('-') {
                self.parse_flag(&value)?;
            } else {
                self.parse_input(&value)?;
            }
        }

        if !self.compiling && !self.preprocessing {
            return Err(CcBypassReason::NotACompile);
        }
        let source = self.source.clone().ok_or(CcBypassReason::MissingInput)?;
        let output = self.output.clone().ok_or(CcBypassReason::MissingOutput)?;
        if self.preprocessing && output == Path::new("-") {
            return Err(CcBypassReason::NonObjectOutput("-E -o -".into()));
        }
        // GCC interprets -o as the dependency output with -E -MD unless -MF
        // is explicit. Do not turn that command into a different operation.
        if self.preprocessing
            && self.dependency.user_headers_only.is_some()
            && self.dependency.file.is_none()
        {
            return Err(CcBypassReason::CallerDependencyFlags(
                "-E -MD/-MMD without -MF".into(),
            ));
        }
        if self.preprocessing && self.dependency.file.as_deref() == Some(Path::new("-")) {
            return Err(CcBypassReason::CallerDependencyFlags("-E -MF -".into()));
        }
        let language = self.language(&source)?;
        let preprocessed_assembly = self.preprocessed_assembly
            || (self.explicit_language.is_none()
                && source.extension().and_then(|value| value.to_str()) == Some("S"));
        self.required_inputs.push(source.clone());
        let caller_depfile = match self.dependency.user_headers_only {
            Some(user_headers_only) => Some(CallerDepfile {
                path: self
                    .dependency
                    .file
                    .unwrap_or_else(|| output.with_extension("d")),
                targets: if self.dependency.targets.is_empty() {
                    vec![DepfileTarget {
                        // GCC -E ignores -o when choosing the dependency
                        // target; Clang retains it. Neither uses the depfile.
                        name: if self.preprocessing && self.family == CcCompilerFamily::Gcc {
                            Path::new(source.file_name().ok_or(CcBypassReason::MissingInput)?)
                                .with_extension("o")
                                .to_string_lossy()
                                .into_owned()
                        } else {
                            output.to_string_lossy().into_owned()
                        },
                        quoted: true,
                    }]
                } else {
                    self.dependency.targets
                },
                user_headers_only,
                phony_targets: self.dependency.phony_targets,
            }),
            // `-MF` or `-MT` without `-MD` or `-MMD` writes nothing, but it is
            // not a shape the cc crate or a makefile produces, so it is not
            // one to guess at.
            None => match self.dependency.modifier {
                Some(modifier) => return Err(CcBypassReason::CallerDependencyFlags(modifier)),
                None => None,
            },
        };
        Ok(CcInvocation {
            arguments: self.parsed,
            source,
            output,
            include_dirs: self.include_dirs,
            required_inputs: self.required_inputs,
            language,
            preprocessed_assembly,
            preprocessing_arguments: self.preprocessing.then(|| {
                self.arguments
                    .iter()
                    .map(|arg| arg.to_str().expect("arguments were validated").to_owned())
                    .collect()
            }),
            sysroot: self.sysroot,
            caller_depfile,
            dependency_argument_indices: self.dependency.indices,
        })
    }

    /// A `-M` flag: the caller wants a dependency list beside the object.
    ///
    /// `-MD` and `-MMD` compile as well as listing, so they are taken, with
    /// the `-MF`, `-MT`, `-MQ`, and `-MP` that shape the list. None of them
    /// enters the key: the object is the same with or without them. `-M` and
    /// `-MM` stop after preprocessing, and `-MG` changes what a missing header
    /// means, so those still bypass.
    fn parse_dependency_flag(&mut self, value: &str, option: &str) -> Result<(), CcBypassReason> {
        let start = self.index - 1;
        match option {
            "D" => self.dependency.user_headers_only = Some(false),
            "MD" => self.dependency.user_headers_only = Some(true),
            "P" => {
                self.dependency.phony_targets = true;
                self.dependency.modifier = Some(value.into());
            }
            _ if option.starts_with('F') => {
                let file = self.take_value("-MF", Some(&option[1..]))?;
                self.dependency.file = Some(PathBuf::from(file));
                self.dependency.modifier = Some("-MF".into());
            }
            _ if option.starts_with('T') || option.starts_with('Q') => {
                let flag = &value[..3];
                let name = self.take_value(flag, Some(&option[1..]))?;
                self.dependency.targets.push(DepfileTarget {
                    name,
                    quoted: option.starts_with('Q'),
                });
                self.dependency.modifier = Some(flag.into());
            }
            _ => return Err(CcBypassReason::CallerDependencyFlags(value.into())),
        }
        self.dependency.indices.extend(start..self.index);
        Ok(())
    }

    fn language(&self, source: &Path) -> Result<CcLanguage, CcBypassReason> {
        if let Some(language) = self.explicit_language {
            return Ok(language);
        }
        let extension = source
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or_default();
        match extension {
            // Preprocessed assembly goes through the C driver: the preprocessor
            // resolves its includes, which is what dependency discovery reads,
            // and the assembler that then produces the object is already part
            // of a GCC identity. Plain `.s` is not preprocessed and reports no
            // dependencies, so it stays out.
            "c" | "S" => Ok(CcLanguage::C),
            "cc" | "cpp" | "cxx" | "c++" => Ok(CcLanguage::Cxx),
            _ => Err(CcBypassReason::UnsupportedLanguage(
                source.display().to_string(),
            )),
        }
    }

    fn current(&self) -> Result<&str, CcBypassReason> {
        self.arguments[self.index]
            .to_str()
            .ok_or(CcBypassReason::NonUtf8Argument { index: self.index })
    }

    fn take_value(&mut self, flag: &str, inline: Option<&str>) -> Result<String, CcBypassReason> {
        if let Some(value) = inline
            && !value.is_empty()
        {
            return Ok(value.into());
        }
        if self.index >= self.arguments.len() {
            return Err(CcBypassReason::MissingValue(flag.into()));
        }
        let value = self.current()?.to_string();
        self.index += 1;
        Ok(value)
    }

    /// `-Xclang -include-pch -Xclang <pch>` or `-Xclang -include -Xclang
    /// <header>`, consumed whole after the leading `-Xclang`. Any other
    /// frontend option, or a pair spelled differently, answers `false` and is
    /// left to the passthrough bypass.
    fn parse_frontend_precompiled_header(&mut self) -> Result<bool, CcBypassReason> {
        let text = |index: usize| {
            self.arguments.get(index).map(|argument| {
                argument
                    .to_str()
                    .ok_or(CcBypassReason::NonUtf8Argument { index })
            })
        };
        let option = match text(self.index).transpose()? {
            Some(option @ ("-include-pch" | "-include")) => option,
            _ => return Ok(false),
        };
        let flag = format!("-Xclang {option}");
        match text(self.index + 1).transpose()? {
            Some("-Xclang") => {}
            Some(_) => return Ok(false),
            None => return Err(CcBypassReason::MissingValue(flag)),
        }
        let path = PathBuf::from(
            text(self.index + 2)
                .transpose()?
                .ok_or_else(|| CcBypassReason::MissingValue(flag.clone()))?,
        );
        if option == "-include-pch" {
            self.required_inputs.push(path.clone());
        }
        self.index += 3;
        self.parsed.push(Argument::Path { flag, path });
        Ok(true)
    }

    fn parse_input(&mut self, value: &str) -> Result<(), CcBypassReason> {
        if self.source.is_some() {
            return Err(CcBypassReason::MultipleInputs);
        }
        let path = PathBuf::from(value);
        // With an explicit `-x`, the driver ignores the extension entirely;
        // without one, the extension is the only thing that decides the
        // language, so an unmodeled extension has to bypass here.
        if self.explicit_language.is_none() {
            let extension = path
                .extension()
                .and_then(|extension| extension.to_str())
                .unwrap_or_default();
            if !matches!(extension, "c" | "S" | "cc" | "cpp" | "cxx" | "c++") {
                return Err(CcBypassReason::UnsupportedLanguage(value.into()));
            }
        }
        self.source = Some(path.clone());
        self.parsed.push(Argument::Source(path));
        Ok(())
    }

    fn parse_flag(&mut self, value: &str) -> Result<(), CcBypassReason> {
        if COMPILER_QUERY_FLAGS.contains(&value) || value.starts_with("-print") {
            return Err(CcBypassReason::CompilerQuery);
        }
        if value == "-E" {
            self.preprocessing = true;
            self.parsed.push(Argument::Plain(value.into()));
            return Ok(());
        }
        if value == "-S" {
            return Err(CcBypassReason::NonObjectOutput(value.into()));
        }
        if let Some(option) = value.strip_prefix("-M") {
            return self.parse_dependency_flag(value, option);
        }
        if value.starts_with("-save-temps") {
            return Err(CcBypassReason::SaveTemps(value.into()));
        }
        if value == "--coverage" {
            return Err(CcBypassReason::CoverageInstrumentation(value.into()));
        }
        if let Some(options) = value.strip_prefix("-Wa,")
            && !options.is_empty()
            && options
                .split(',')
                .all(|option| SUPPORTED_ASSEMBLER_OPTIONS.contains(&option))
        {
            self.parsed.push(Argument::Plain(value.into()));
            return Ok(());
        }
        // A precompiled header used rather than built: `-include-pch <pch>` to
        // the driver, or the same option handed straight to clang's frontend,
        // which is how Bun's build spells it. The frontend reads the PCH whole,
        // and the dependency list names the headers recorded inside it but
        // never the PCH itself, so the PCH is a required input keyed by its
        // bytes. A PCH records where and when its headers were, so one rebuilt
        // elsewhere has other bytes and the compilations using it miss there.
        if value == "-Xclang" && self.parse_frontend_precompiled_header()? {
            return Ok(());
        }
        if value == "-include-pch" {
            let path = PathBuf::from(self.take_value(value, None)?);
            self.required_inputs.push(path.clone());
            self.parsed.push(Argument::Path {
                flag: value.into(),
                path,
            });
            return Ok(());
        }
        if TOOL_PASSTHROUGH_FLAGS.contains(&value)
            || value.starts_with("-Wp,")
            || value.starts_with("-Wa,")
            || value.starts_with("-Wl,")
        {
            // The forwarded options are the compilation's real inputs and they
            // are not modeled, so consuming the value would not make this safe.
            return Err(CcBypassReason::ToolPassthrough(value.into()));
        }
        if value.starts_with("-include-pch") || value == "-emit-pch" {
            return Err(CcBypassReason::PrecompiledHeader(value.into()));
        }

        if value == "-c" {
            self.compiling = true;
            self.parsed.push(Argument::Plain(value.into()));
            return Ok(());
        }
        if SUPPORTED_BARE_FLAGS.contains(&value)
            || SUPPORTED_O_FLAGS.contains(&value)
            || SUPPORTED_G_FLAGS.contains(&value)
            || value.starts_with("-std=")
        {
            self.parsed.push(Argument::Plain(value.into()));
            return Ok(());
        }
        if let Some(rest) = value.strip_prefix("-o") {
            let path = self.take_value("-o", Some(rest))?;
            // A repeated `-o` follows the driver: the last one names the file
            // that is produced. Every occurrence still enters the key.
            self.output = Some(PathBuf::from(&path));
            self.parsed.push(Argument::Path {
                flag: "-o".into(),
                path: PathBuf::from(path),
            });
            return Ok(());
        }
        if let Some(rest) = value.strip_prefix("-I") {
            let path = PathBuf::from(self.take_value("-I", Some(rest))?);
            self.include_dirs.push(path.clone());
            self.parsed.push(Argument::Path {
                flag: "-I".into(),
                path,
            });
            return Ok(());
        }
        if SEPARATE_PATH_FLAGS.contains(&value) {
            let path = PathBuf::from(self.take_value(value, None)?);
            match value {
                "-isystem" | "-iquote" | "-idirafter" => self.include_dirs.push(path.clone()),
                "-isysroot" => self.sysroot = Some(path.clone()),
                // `-include` and `-imacros` are deliberately not required
                // inputs. The driver resolves the name through the include
                // chain, so the file need not exist relative to the working
                // directory, and the dependency list names it at whatever path
                // it was actually found at.
                _ => {}
            }
            self.parsed.push(Argument::Path {
                flag: value.into(),
                path,
            });
            return Ok(());
        }
        // `--include=<file>` is the long spelling of `-include <file>`; the
        // `cc` crate emits it for prefixed headers.
        if let Some(rest) = value.strip_prefix("--include=") {
            let path = PathBuf::from(rest);
            self.parsed.push(Argument::Path {
                flag: "-include".into(),
                path,
            });
            return Ok(());
        }
        if let Some((flag, rest)) = PREFIX_MAP_FLAGS.iter().find_map(|flag| {
            value
                .strip_prefix(&format!("{flag}="))
                .map(|rest| (*flag, rest))
        }) {
            let (from, to) = rest.split_once('=').unwrap_or((rest, ""));
            self.parsed.push(Argument::PrefixMap {
                flag: flag.into(),
                from: PathBuf::from(from),
                to: to.into(),
            });
            return Ok(());
        }
        // `--param name=value` tunes the optimizer; its text fully describes it.
        if value == "--param" {
            let parameter = self.take_value("--param", None)?;
            self.parsed
                .push(Argument::Plain(format!("--param={parameter}")));
            return Ok(());
        }
        if let Some(parameter) = value.strip_prefix("--param=") {
            self.parsed
                .push(Argument::Plain(format!("--param={parameter}")));
            return Ok(());
        }
        if let Some(rest) = value.strip_prefix("--sysroot=") {
            let path = PathBuf::from(rest);
            self.sysroot = Some(path.clone());
            self.parsed.push(Argument::Path {
                flag: "--sysroot".into(),
                path,
            });
            return Ok(());
        }
        if let Some(rest) = value
            .strip_prefix("-D")
            .or_else(|| value.strip_prefix("-U"))
        {
            let flag = &value[..2];
            let definition = self.take_value(flag, Some(rest))?;
            self.parsed
                .push(Argument::Plain(format!("{flag}{definition}")));
            return Ok(());
        }
        if let Some(rest) = value.strip_prefix("-x") {
            let language = self.take_value("-x", Some(rest))?;
            self.explicit_language = Some(match language.as_str() {
                "c" => CcLanguage::C,
                "assembler-with-cpp" => {
                    self.preprocessed_assembly = true;
                    CcLanguage::C
                }
                "c++" => CcLanguage::Cxx,
                other => return Err(CcBypassReason::UnsupportedLanguage(other.into())),
            });
            self.parsed.push(Argument::Plain(format!("-x{language}")));
            return Ok(());
        }
        if let Some(target) = value.strip_prefix("--target=") {
            self.parsed
                .push(Argument::Plain(format!("--target={target}")));
            return Ok(());
        }
        if value == "-target" {
            let target = self.take_value("-target", None)?;
            self.parsed
                .push(Argument::Plain(format!("--target={target}")));
            return Ok(());
        }
        if value == "-arch" {
            let arch = self.take_value("-arch", None)?;
            self.parsed.push(Argument::Plain(format!("-arch={arch}")));
            return Ok(());
        }
        if let Some(option) = value.strip_prefix("-f") {
            return self.parse_f_flag(value, option);
        }
        if let Some(option) = value.strip_prefix("-m") {
            return self.parse_m_flag(value, option);
        }
        if value.starts_with("-g") {
            // `-gsplit-dwarf` writes a `.dwo` beside the object; every other
            // unlisted `-g` spelling is simply unmodeled.
            return Err(if value.starts_with("-gsplit-dwarf") {
                CcBypassReason::SplitDebugOutput(value.into())
            } else {
                CcBypassReason::UnknownFlag(value.into())
            });
        }
        if value.starts_with("-W") {
            // Warning selection changes only diagnostics, which are replayed
            // from the cache, and the exit status, and only successful
            // compiles are ever published.
            self.parsed.push(Argument::Plain(value.into()));
            return Ok(());
        }
        Err(CcBypassReason::UnknownFlag(value.into()))
    }

    fn parse_f_flag(&mut self, value: &str, option: &str) -> Result<(), CcBypassReason> {
        if option.starts_with("plugin") || option.starts_with("pass-plugin") {
            return Err(CcBypassReason::Plugin(value.into()));
        }
        if option.starts_with("profile-") || option == "test-coverage" {
            return Err(CcBypassReason::CoverageInstrumentation(value.into()));
        }
        let name = option.split_once('=').map_or(option, |(name, _)| name);
        if SUPPORTED_F_FLAGS.binary_search(&name).is_err() {
            return Err(CcBypassReason::UnknownFlag(value.into()));
        }
        self.parsed.push(Argument::Plain(value.into()));
        Ok(())
    }

    fn parse_m_flag(&mut self, value: &str, option: &str) -> Result<(), CcBypassReason> {
        if option == "llvm" {
            return Err(CcBypassReason::ToolPassthrough(value.into()));
        }
        // `-march=native` and its relatives resolve against whatever CPU this
        // machine has. The resulting object is not a function of the key, so
        // another machine could otherwise restore code its processor cannot
        // run.
        if let Some((name, selection)) = option.split_once('=')
            && matches!(name, "arch" | "cpu" | "tune")
            && matches!(selection, "native" | "host")
        {
            return Err(CcBypassReason::LocalCpuTarget(value.into()));
        }
        let name = option.split_once('=').map_or(option, |(name, _)| name);
        if SUPPORTED_M_FLAGS.binary_search(&name).is_err() {
            return Err(CcBypassReason::UnknownFlag(value.into()));
        }
        self.parsed.push(Argument::Plain(value.into()));
        Ok(())
    }
}

/// Conservative parser for the command lines emitted by the `cc` crate for
/// Microsoft's compiler. It intentionally admits only flags whose effects are
/// either present in argv or covered by dependency discovery.
struct MsvcParser<'a> {
    arguments: &'a [OsString],
    index: usize,
    parsed: Vec<Argument>,
    source: Option<PathBuf>,
    output: Option<PathBuf>,
    include_dirs: Vec<PathBuf>,
    required_inputs: Vec<PathBuf>,
    explicit_language: Option<CcLanguage>,
    compiling: bool,
}

impl<'a> MsvcParser<'a> {
    fn new(arguments: &'a [OsString]) -> Self {
        Self {
            arguments,
            index: 0,
            parsed: Vec::new(),
            source: None,
            output: None,
            include_dirs: Vec::new(),
            required_inputs: Vec::new(),
            explicit_language: None,
            compiling: false,
        }
    }

    fn parse(mut self) -> Result<CcInvocation, CcBypassReason> {
        while self.index < self.arguments.len() {
            let value = self.current()?.to_owned();
            self.index += 1;
            if let Some(file) = value.strip_prefix('@') {
                return Err(CcBypassReason::ResponseFile(file.into()));
            }
            if value.starts_with('/') || value.starts_with('-') {
                self.parse_flag(&value)?;
            } else {
                self.add_source(&value)?;
            }
        }
        if !self.compiling {
            return Err(CcBypassReason::NotACompile);
        }
        let source = self.source.clone().ok_or(CcBypassReason::MissingInput)?;
        let output = self.output.clone().ok_or(CcBypassReason::MissingOutput)?;
        let language = self.explicit_language.unwrap_or_else(|| {
            if source
                .extension()
                .and_then(|value| value.to_str())
                .is_some_and(|value| value.eq_ignore_ascii_case("c"))
            {
                CcLanguage::C
            } else {
                CcLanguage::Cxx
            }
        });
        self.required_inputs.push(source.clone());
        Ok(CcInvocation {
            arguments: self.parsed,
            source,
            output,
            include_dirs: self.include_dirs,
            required_inputs: self.required_inputs,
            language,
            preprocessed_assembly: false,
            preprocessing_arguments: None,
            sysroot: None,
            caller_depfile: None,
            dependency_argument_indices: Vec::new(),
        })
    }

    fn current(&self) -> Result<&str, CcBypassReason> {
        self.arguments[self.index]
            .to_str()
            .ok_or(CcBypassReason::NonUtf8Argument { index: self.index })
    }

    fn value(&mut self, flag: &str, attached: &str) -> Result<String, CcBypassReason> {
        if !attached.is_empty() {
            return Ok(attached.into());
        }
        if self.index == self.arguments.len() {
            return Err(CcBypassReason::MissingValue(flag.into()));
        }
        let value = self.current()?.to_owned();
        self.index += 1;
        Ok(value)
    }

    fn add_source(&mut self, value: &str) -> Result<(), CcBypassReason> {
        if self.source.is_some() {
            return Err(CcBypassReason::MultipleInputs);
        }
        let path = PathBuf::from(value);
        if self.explicit_language.is_none()
            && !path
                .extension()
                .and_then(|value| value.to_str())
                .is_some_and(|value| {
                    matches!(
                        value.to_ascii_lowercase().as_str(),
                        "c" | "cc" | "cpp" | "cxx"
                    )
                })
        {
            return Err(CcBypassReason::UnsupportedLanguage(value.into()));
        }
        self.source = Some(path.clone());
        self.parsed.push(Argument::Source(path));
        Ok(())
    }

    fn parse_flag(&mut self, value: &str) -> Result<(), CcBypassReason> {
        let option = value.trim_start_matches(['/', '-']);
        let lower = option.to_ascii_lowercase();
        if matches!(lower.as_str(), "?" | "help") {
            return Err(CcBypassReason::CompilerQuery);
        }
        if lower == "showincludes" || lower.starts_with("sourcedependencies") {
            return Err(CcBypassReason::CallerDependencyFlags(value.into()));
        }
        if matches!(lower.as_str(), "e" | "ep" | "p") {
            return Err(CcBypassReason::NonObjectOutput(value.into()));
        }
        if (lower.starts_with("fa") && !lower.starts_with("favor:"))
            || lower.starts_with("fd")
            || lower.starts_with("zi")
        {
            return Err(CcBypassReason::SplitDebugOutput(value.into()));
        }
        if lower.starts_with("yc")
            || lower.starts_with("yu")
            || (lower.starts_with("fp") && !lower.starts_with("fp:"))
        {
            return Err(CcBypassReason::PrecompiledHeader(value.into()));
        }
        if lower == "link" || lower.starts_with("bt+") || lower.starts_with("analyze") {
            return Err(CcBypassReason::ToolPassthrough(value.into()));
        }
        if matches!(lower.as_str(), "ld" | "ldd") {
            return Err(CcBypassReason::NotACompile);
        }
        if lower == "c" {
            self.compiling = true;
            self.parsed.push(Argument::Plain("/c".into()));
            return Ok(());
        }
        for (prefix, canonical) in [("Fo", "/Fo"), ("I", "/I"), ("FI", "/FI")] {
            if let Some(attached) = option.strip_prefix(prefix) {
                let path = PathBuf::from(self.value(canonical, attached)?);
                if prefix == "Fo" {
                    self.output = Some(path.clone());
                } else if prefix == "I" {
                    self.include_dirs.push(path.clone());
                }
                self.parsed.push(Argument::Path {
                    flag: canonical.into(),
                    path,
                });
                return Ok(());
            }
        }
        if lower.starts_with("external:i") {
            let path = PathBuf::from(self.value("/external:I", &option[10..])?);
            self.include_dirs.push(path.clone());
            self.parsed.push(Argument::Path {
                flag: "/external:I".into(),
                path,
            });
            return Ok(());
        }
        if option.starts_with("Tc") || option.starts_with("Tp") {
            let c = option.starts_with("Tc");
            let path = self.value(if c { "/Tc" } else { "/Tp" }, &option[2..])?;
            self.explicit_language = Some(if c { CcLanguage::C } else { CcLanguage::Cxx });
            return self.add_source(&path);
        }
        if lower.starts_with("pathmap:") {
            let rest = &option[8..];
            let (from, to) = rest.split_once('=').unwrap_or((rest, ""));
            self.parsed.push(Argument::PrefixMap {
                flag: "/pathmap".into(),
                from: PathBuf::from(from),
                to: to.into(),
            });
            return Ok(());
        }
        if matches!(option, "D" | "U") {
            let definition = self.value(value, "")?;
            self.parsed
                .push(Argument::Plain(format!("/{option}{definition}")));
            return Ok(());
        }
        // Definitions and the ordinary code-generation/diagnostic switches
        // produced by cc-rs are self-contained text and can be keyed verbatim.
        let definition = option.starts_with('D') || option.starts_with('U');
        let warning = matches!(lower.as_str(), "wall" | "wx" | "wx-")
            || lower
                .strip_prefix('w')
                .is_some_and(|value| value.bytes().all(|byte| byte.is_ascii_digit()))
            || ["wd", "we", "wo"].iter().any(|prefix| {
                lower
                    .strip_prefix(prefix)
                    .is_some_and(|value| value.bytes().all(|byte| byte.is_ascii_digit()))
            });
        let admitted = definition
            || warning
            || lower.starts_with("std:")
            || lower.starts_with("arch:")
            || lower.starts_with("favor:")
            || lower.starts_with("volatile:")
            || lower.starts_with("fp:")
            || lower.starts_with("eh")
            || lower.starts_with('o')
            || lower.starts_with("ob")
            || lower.starts_with("oi")
            || lower.starts_with("ot")
            || lower.starts_with("oy")
            || lower.starts_with("gs")
            || lower.starts_with("gr")
            || lower.starts_with("gy")
            || lower.starts_with("gw")
            || lower.starts_with("gl")
            || lower.starts_with("zc:")
            || lower.starts_with("diagnostics:")
            || matches!(
                lower.as_str(),
                "nologo"
                    | "brepro"
                    | "bigobj"
                    | "utf-8"
                    | "permissive-"
                    | "z7"
                    | "md"
                    | "mdd"
                    | "mt"
                    | "mtd"
            );
        if admitted {
            self.parsed.push(Argument::Plain(value.into()));
            return Ok(());
        }
        Err(CcBypassReason::UnknownFlag(value.into()))
    }
}

#[cfg(test)]
#[path = "cc_cache_tests.rs"]
mod tests;
