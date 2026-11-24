//! `oneios-tool debug NAME`: the helpers of the comparison scripts under tools/oneios-tool/tests. Each was a
//! Python script beside its test until 2026-11-13 (CLAUDE.md: no new Python); each module has that script's name
//! and description, takes what it took and prints what it printed.
//!
//!   kcmap.rs          tests/ksym/kcmap.py          the test's own reader of a kernel collection
//!   ksym_make_log.rs  tests/ksym/make-log.py       a stand-in panic log for a collection and its kernel
//!   ksym_compare.rs   tests/ksym/compare.py        `sym kernel` against llvm-symbolizer
//!   kernel_diff.rs    tests/kernelgen/kernel-diff.py   where two builds of the hosted kernel differ
//!   mkstream.rs       tests/phone/mkstream.py      the streams of tests/phone/sparse-compare.sh
//!   pyrandom.rs       Python's random.Random, for the sample and the streams
//!   stub_kc.rs        new: a made-up kernel collection for the tests of `prelink stubs-check`
//!   fixture.rs        new: a made-up kernel and a made-up KernelDebugKit.pkg for tests/kernelpy
//!   mkapfs            tests/ioslog/mkapfs.py       a raw APFS image from folders (the file is tests/support/mkapfs.rs,
//!                                                  which the crates' own tests include too)
//!   mkapk.rs          tests/app/mkapk.py           a stand-in APK around a certificate
//!   ioslog_filters.rs tests/ioslog/filters.py      `ioslog show`'s filters against a reference CSV
//!   sym_log.rs        tests/sym/make-log.py and the address-set block of tests/sym/compare-golden.sh
//!   mktar.rs          new (userland/tests/test_helpers.py's `seed()`): a tar for the oneios-seed tests
//!   holes.rs          new (an inline block of tests/phone/sparse-compare.sh): what the file system holds for a file

pub mod fixture;
pub mod holes;
pub mod ioslog_filters;
pub mod kcmap;
pub mod kernel_diff;
pub mod ksym_compare;
pub mod ksym_make_log;
#[path = "../../../tests/support/mkapfs.rs"]
pub mod mkapfs;
pub mod mkapk;
pub mod mkstream;
pub mod mktar;
pub mod pyrandom;
pub mod stub_kc;
pub mod sym_log;

#[derive(clap::Subcommand)]
pub enum Command {
    /// The test's own reader of a kernel collection: members KC | forward KC KERNEL NAME... | own KC NAME...
    Kcmap {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// A stand-in xnuhost session log with the panic of 2026-11-12, for a collection and its kernel
    KsymMakeLog {
        kc: String,
        kernel: String,
        /// One line per address written: KIND ADDRESS-IN-LOG KERNEL-ADDRESS FUNCTION
        #[arg(long)]
        expect: Option<String>,
    },
    /// `sym kernel` against llvm-symbolizer, address by address (tests/ksym/compare.sh runs it)
    KsymCompare(ksym_compare::Args),
    /// Where two builds of the hosted kernel differ: only the build's time, the UUID, the N_OSO times, the seal?
    KernelDiff { a: String, b: String },
    /// A made-up kernel collection with GOOD good stubs and BAD bad ones (the tests of `prelink stubs-check`)
    StubKc {
        out: String,
        good: u64,
        bad: u64,
        /// Appended after the file's last byte
        note: Option<String>,
    },
    /// A made-up input: hosted-kernel | kdk-pkg (tests/kernelpy), guest-bundle | guest-dylib | guest-execute | apple-dylib (runtime/package/test.sh)
    Fixture { name: String, out: String },
    /// The streams of tests/phone/sparse-compare.sh: runs|tail|zero|empty|lastbyte|dense | phone MIB DATA
    Mkstream {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// A raw APFS image from folders: OUT.img NAME=FOLDER [NAME=FOLDER ...] [--stale-block0]
    Mkapfs {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// A stand-in APK (a zip and an APK Signing Block v2) around CERT.der, for tests/app/run.sh
    Mkapk { out: String, cert: String, text: Option<String> },
    /// A tar from a spec on stdin (file|dir|symlink|hard NAME [mode= uid= gid= target= data=PATH pax=K=V]...)
    Mktar {
        #[arg(long, default_value = "pax")]
        format: String,
        out: String,
    },
    /// `ioslog show`'s filters against the rows of unifiedlog_iterator's CSV that each rule keeps
    IoslogFilters { tool: String, source: String, reference: String, work: String },
    /// A stand-in xnuhost log from a dyld shared cache: DSC_DIR OUT.log [--slide HEX] [--stacks N] [--seed N]
    SymMakeLog {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// The address sets of tests/sym/compare-golden.sh from its stand-in logs: WORK LOG...
    SymCases { work: String, logs: Vec<String> },
    /// What the file system holds for a file: LENGTH ALLOCATED REGIONS DATA (--map: each data region)
    Holes {
        file: String,
        #[arg(long)]
        map: bool,
    },
}

/// A helper's message ends on stderr with status 1, as the scripts' `SystemExit(message)` did.
pub fn main(command: Command) -> i32 {
    let done = match command {
        Command::Kcmap { args } => kcmap::run(&args),
        Command::KsymMakeLog { kc, kernel, expect } => ksym_make_log::run(&kc, &kernel, expect.as_deref()),
        Command::KsymCompare(args) => ksym_compare::run(&args),
        Command::KernelDiff { a, b } => kernel_diff::run(&a, &b),
        Command::Mkstream { args } => Ok(mkstream::run(&args)),
        Command::Fixture { name, out } => fixture::run(&name, &out),
        Command::StubKc { out, good, bad, note } => stub_kc::run(&out, good, bad, note.as_deref().unwrap_or("")),
        Command::Mkapfs { args } => match mkapfs::run(&args) {
            Ok(line) => {
                println!("{line}");
                Ok(0)
            }
            Err(message) => Err(anyhow::anyhow!(message)),
        },
        Command::Mkapk { out, cert, text } => mkapk::run(&out, &cert, text.as_deref()),
        Command::Mktar { format, out } => mktar::run(&format, &out),
        Command::IoslogFilters { tool, source, reference, work } => ioslog_filters::run(&tool, &source, &reference, &work),
        Command::SymMakeLog { args } => sym_log::make_log(&args),
        Command::SymCases { work, logs } => sym_log::cases(&work, &logs),
        Command::Holes { file, map } => holes::run(&file, map),
    };
    done.unwrap_or_else(|e| {
        eprintln!("{e:#}");
        1
    })
}
