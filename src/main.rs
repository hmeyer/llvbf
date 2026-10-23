use anyhow::{bail, Context, Result};
use std::path::PathBuf;

fn usage() -> ! {
    eprintln!(
        "usage:
  llvbf compile <input.ll|input.bc> [-o out.bf] [--mem-size N] [--comments] [--entry NAME]
  llvbf cc <sources.c...> [-o out.bf] [--run] [--target TRIPLE] [-O0|-O1|-O2|-O3|-Os|-Oz]
           [--mem-size N] [--comments] [--entry NAME] [--runtime DIR] [--no-runtime]
           [--keep-ir out.ll] [--llvm-bin DIR] [-I dir] [-D macro] [-Xclang ARG]
  llvbf run <program.bf> [--max-steps N] [--stats] [--profile]
  llvbf size <program.bf>      (bytes per construct; needs a --comments build)"
    );
    std::process::exit(2);
}

fn main() {
    if let Err(e) = real_main() {
        eprintln!("error: {:#}", e);
        std::process::exit(1);
    }
}

fn real_main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        usage();
    }
    match args[1].as_str() {
        "compile" => {
            let input = PathBuf::from(&args[2]);
            let mut out: Option<PathBuf> = None;
            let mut opts = llvbf::codegen::Options::default();
            let mut i = 3;
            while i < args.len() {
                match args[i].as_str() {
                    "-o" => {
                        out = Some(PathBuf::from(args.get(i + 1).unwrap_or_else(|| usage())));
                        i += 1;
                    }
                    "--mem-size" => {
                        opts.mem_size = args.get(i + 1).unwrap_or_else(|| usage()).parse()?;
                        i += 1;
                    }
                    "--entry" => {
                        opts.entry = args.get(i + 1).unwrap_or_else(|| usage()).clone();
                        i += 1;
                    }
                    "--comments" => opts.comments = true,
                    _ => usage(),
                }
                i += 1;
            }
            let code = llvbf::compile_file(&input, &opts)?;
            match out {
                Some(p) => {
                    std::fs::write(&p, code).with_context(|| format!("writing {}", p.display()))?
                }
                None => {
                    use std::io::Write;
                    std::io::stdout().write_all(&code)?;
                }
            }
        }
        "cc" => {
            let mut opts = llvbf::driver::CcOptions::default();
            let mut out: Option<PathBuf> = None;
            let mut run_after = false;
            let mut i = 2;
            while i < args.len() {
                let a = args[i].as_str();
                match a {
                    "-o" => {
                        out = Some(PathBuf::from(args.get(i + 1).unwrap_or_else(|| usage())));
                        i += 1;
                    }
                    "--run" => run_after = true,
                    "--target" => {
                        opts.target = args.get(i + 1).unwrap_or_else(|| usage()).clone();
                        i += 1;
                    }
                    "-O0" | "-O1" | "-O2" | "-O3" | "-Os" | "-Oz" => opts.opt_level = a.to_string(),
                    "--mem-size" => {
                        opts.codegen.mem_size =
                            args.get(i + 1).unwrap_or_else(|| usage()).parse()?;
                        i += 1;
                    }
                    "--comments" => opts.codegen.comments = true,
                    "--entry" => {
                        opts.codegen.entry = args.get(i + 1).unwrap_or_else(|| usage()).clone();
                        i += 1;
                    }
                    "--runtime" => {
                        opts.runtime_dir =
                            Some(PathBuf::from(args.get(i + 1).unwrap_or_else(|| usage())));
                        i += 1;
                    }
                    "--no-runtime" => opts.no_runtime = true,
                    "--keep-ir" => {
                        opts.keep_ir =
                            Some(PathBuf::from(args.get(i + 1).unwrap_or_else(|| usage())));
                        i += 1;
                    }
                    "--llvm-bin" => {
                        opts.llvm_bin =
                            Some(PathBuf::from(args.get(i + 1).unwrap_or_else(|| usage())));
                        i += 1;
                    }
                    "-I" | "-D" | "-Xclang" | "-include" => {
                        if a != "-Xclang" {
                            opts.extra_clang_args.push(a.to_string());
                        }
                        opts.extra_clang_args
                            .push(args.get(i + 1).unwrap_or_else(|| usage()).clone());
                        i += 1;
                    }
                    _ if a.starts_with("-I")
                        || a.starts_with("-D")
                        || a.starts_with("-W")
                        || a.starts_with("-f")
                        || a.starts_with("-std=") =>
                    {
                        opts.extra_clang_args.push(a.to_string());
                    }
                    _ if a.starts_with('-') => usage(),
                    _ => opts.sources.push(PathBuf::from(a)),
                }
                i += 1;
            }
            let code = llvbf::driver::cc(&opts)?;
            if let Some(p) = &out {
                std::fs::write(p, &code).with_context(|| format!("writing {}", p.display()))?;
            }
            if run_after {
                let src = String::from_utf8_lossy(&code).into_owned();
                let prog = llvbf::bfi::Program::parse(&src)?;
                let stdin = std::io::stdin();
                let stdout = std::io::stdout();
                let mut inp = stdin.lock();
                let mut o = stdout.lock();
                prog.run(&mut inp, &mut o, None)?;
            } else if out.is_none() {
                use std::io::Write;
                std::io::stdout().write_all(&code)?;
            }
        }
        "size" => {
            let src = std::fs::read_to_string(&args[2])
                .with_context(|| format!("reading {}", args[2]))?;
            llvbf::size_report(&src);
        }
        "run" => {
            let src = std::fs::read_to_string(&args[2])
                .with_context(|| format!("reading {}", args[2]))?;
            let mut max_steps = None;
            let mut stats = false;
            let mut profile = false;
            let mut i = 3;
            while i < args.len() {
                match args[i].as_str() {
                    "--max-steps" => {
                        max_steps = Some(args.get(i + 1).unwrap_or_else(|| usage()).parse()?);
                        i += 1;
                    }
                    "--stats" => stats = true,
                    "--profile" => profile = true,
                    _ => usage(),
                }
                i += 1;
            }
            let prog = llvbf::bfi::Program::parse(&src)?;
            if profile {
                // Attribute executed ops to the `# ...` comments of a program compiled with --comments.
                let input = {
                    use std::io::Read;
                    let mut v = Vec::new();
                    std::io::stdin().read_to_end(&mut v)?;
                    v
                };
                let counts = prog.profile(&input, max_steps)?;
                let total: u64 = counts.iter().map(|c| c.1).sum();
                eprintln!("[llvbf] {} ops; top regions:", total);
                for (name, c) in counts.iter().take(30) {
                    eprintln!(
                        "{:>12} {:5.1}%  {}",
                        c,
                        100.0 * *c as f64 / total as f64,
                        name
                    );
                }
                return Ok(());
            }
            let stdin = std::io::stdin();
            let stdout = std::io::stdout();
            let mut inp = stdin.lock();
            let mut out = stdout.lock();
            let st = prog.run(&mut inp, &mut out, max_steps)?;
            if stats {
                eprintln!(
                    "[llvbf] {} ops executed, {} tape cells used, {} compiled ops",
                    st.steps,
                    st.max_cell + 1,
                    prog.len()
                );
            }
        }
        _ => bail!("unknown command {}", args[1]),
    }
    Ok(())
}
