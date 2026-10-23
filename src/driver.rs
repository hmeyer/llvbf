//! The `llvbf cc` driver: compiles C sources with clang for a 16-bit target,
//! links them with the llvbf runtime, optimizes and lowers to Brainfuck.

use anyhow::{anyhow, bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct CcOptions {
    pub sources: Vec<PathBuf>,
    pub output: Option<PathBuf>,
    pub target: String,
    pub opt_level: String,
    pub extra_clang_args: Vec<String>,
    pub runtime_dir: Option<PathBuf>,
    pub no_runtime: bool,
    pub keep_ir: Option<PathBuf>,
    pub llvm_bin: Option<PathBuf>,
    pub codegen: crate::codegen::Options,
}

impl Default for CcOptions {
    fn default() -> Self {
        CcOptions {
            sources: vec![],
            output: None,
            target: "msp430".to_string(),
            opt_level: "-Os".to_string(),
            extra_clang_args: vec![],
            runtime_dir: None,
            no_runtime: false,
            keep_ir: None,
            llvm_bin: None,
            codegen: crate::codegen::Options::default(),
        }
    }
}

/// The LLVM major version llvbf is built against.
pub const LLVM_VERSION: &str = "23";

/// Locate an LLVM tool: `--llvm-bin`, then `LLVBF_LLVM_BIN`, then the versioned
/// installation (`/usr/lib/llvm-23/bin`, `clang-23`), then the plain name.
fn find_tool(bin: &Option<PathBuf>, name: &str) -> Result<PathBuf> {
    let versioned = format!("{}-{}", name, LLVM_VERSION);
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(dir) = bin {
        candidates.push(dir.join(name));
        candidates.push(dir.join(&versioned));
    } else {
        if let Ok(p) = std::env::var("LLVBF_LLVM_BIN") {
            candidates.push(PathBuf::from(&p).join(name));
            candidates.push(PathBuf::from(&p).join(&versioned));
        }
        candidates.push(PathBuf::from(format!(
            "/usr/lib/llvm-{}/bin/{}",
            LLVM_VERSION, name
        )));
        candidates.push(PathBuf::from(&versioned));
        candidates.push(PathBuf::from(name));
    }
    for c in candidates {
        if Command::new(&c)
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            return Ok(c);
        }
    }
    bail!(
        "cannot find LLVM {} tool '{}' (set LLVBF_LLVM_BIN or --llvm-bin)",
        LLVM_VERSION,
        name
    )
}

pub fn default_runtime_dir() -> PathBuf {
    if let Ok(p) = std::env::var("LLVBF_RUNTIME") {
        return PathBuf::from(p);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("runtime")
}

fn run(cmd: &mut Command) -> Result<()> {
    let out = cmd.output().with_context(|| format!("running {:?}", cmd))?;
    if !out.status.success() {
        bail!(
            "command failed: {:?}\n{}{}",
            cmd,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let err = String::from_utf8_lossy(&out.stderr);
    if !err.trim().is_empty() {
        eprint!("{}", err);
    }
    Ok(())
}

/// Compile C sources to a single optimized bitcode file. Returns its path.
pub fn compile_to_bitcode(opts: &CcOptions, work: &Path) -> Result<PathBuf> {
    let clang = find_tool(&opts.llvm_bin, "clang")?;
    let llvm_link = find_tool(&opts.llvm_bin, "llvm-link")?;
    let opt = find_tool(&opts.llvm_bin, "opt")?;
    let rt = opts.runtime_dir.clone().unwrap_or_else(default_runtime_dir);
    let include = rt.join("include");
    let mut objects: Vec<PathBuf> = Vec::new();
    let mut sources = opts.sources.clone();
    if !opts.no_runtime {
        for f in ["io.c", "string.c", "stdlib.c", "ctype.c"] {
            let p = rt.join(f);
            if !p.exists() {
                bail!(
                    "runtime file {} not found (set LLVBF_RUNTIME or --runtime)",
                    p.display()
                );
            }
            sources.push(p);
        }
    }
    for (i, src) in sources.iter().enumerate() {
        let obj = work.join(format!(
            "{}_{}.bc",
            i,
            src.file_stem().and_then(|s| s.to_str()).unwrap_or("src")
        ));
        let mut cmd = Command::new(&clang);
        cmd.arg("-target")
            .arg(&opts.target)
            .arg(&opts.opt_level)
            .arg("-ffreestanding")
            .arg("-nostdlibinc")
            .arg("-fno-builtin-printf")
            .arg("-fno-unroll-loops")
            .arg("-fno-vectorize")
            .arg("-fno-slp-vectorize")
            .arg("-Wno-unused-command-line-argument")
            .arg("-isystem")
            .arg(&include)
            .arg("-emit-llvm")
            .arg("-c");
        let ext = src.extension().and_then(|e| e.to_str()).unwrap_or("");
        if ext == "ll" || ext == "bc" {
            // Already IR: just pass through.
            std::fs::copy(src, &obj)?;
            objects.push(obj);
            continue;
        }
        for a in &opts.extra_clang_args {
            cmd.arg(a);
        }
        cmd.arg(src).arg("-o").arg(&obj);
        run(&mut cmd)?;
        objects.push(obj);
    }
    let linked = work.join("linked.bc");
    let mut cmd = Command::new(&llvm_link);
    cmd.args(&objects).arg("-o").arg(&linked);
    run(&mut cmd)?;
    let optimized = work.join("optimized.bc");
    let mut cmd = Command::new(&opt);
    // Size levels are expressed through the `optsize`/`minsize` function attributes
    // that clang already set; the pipeline itself is O2 (opt rejects Os/Oz).
    let level = match opts.opt_level.as_str() {
        "-O0" => "default<O0>",
        "-O1" => "default<O1>",
        "-O3" => "default<O3>",
        _ => "default<O2>",
    };
    cmd.arg(format!("-passes=internalize,globaldce,{},globaldce", level))
        .arg(format!(
            "-internalize-public-api-list={},memcpy,memmove,memset",
            opts.codegen.entry
        ))
        .arg(&linked)
        .arg("-o")
        .arg(&optimized);
    run(&mut cmd)?;
    if let Some(keep) = &opts.keep_ir {
        let dis = find_tool(&opts.llvm_bin, "llvm-dis")?;
        let mut cmd = Command::new(&dis);
        cmd.arg(&optimized).arg("-o").arg(keep);
        run(&mut cmd)?;
    }
    Ok(optimized)
}

/// Full pipeline: C sources to Brainfuck.
pub fn cc(opts: &CcOptions) -> Result<Vec<u8>> {
    if opts.sources.is_empty() {
        bail!("no input files");
    }
    let work = tempdir()?;
    let bc = compile_to_bitcode(opts, &work)?;
    let code = crate::compile_file(&bc, &opts.codegen)?;
    let _ = std::fs::remove_dir_all(&work);
    Ok(code)
}

fn tempdir() -> Result<PathBuf> {
    let base = std::env::temp_dir();
    let pid = std::process::id();
    for i in 0..1000 {
        let p = base.join(format!("llvbf-{}-{}", pid, i));
        if std::fs::create_dir(&p).is_ok() {
            return Ok(p);
        }
    }
    Err(anyhow!("cannot create a temporary directory"))
}
