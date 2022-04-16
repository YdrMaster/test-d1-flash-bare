use clap::{Parser, Subcommand};
use std::env;
use std::path::{Path, PathBuf};
use std::process::{self, Command, Stdio};
use std::fs::File;
use std::io::{SeekFrom, Seek, ErrorKind};
use byteorder::{LittleEndian, WriteBytesExt, ReadBytesExt};

#[derive(Parser)]
#[clap(name = "xtask")]
#[clap(about = "Program that help you build and debug d1 flash test project", long_about = None)]
struct Cli {
    #[clap(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Build ELF and binary for test flash
    Make {
        #[clap(flatten)]
        env: Env,
    },
    /// View test flash assembly code
    Asm {
        #[clap(flatten)]
        env: Env,
    },
}

#[derive(clap::Args)]
struct Env {
    #[clap(
        long = "release",
        global = true,
        help = "Build in release mode",
        long_help = None,
    )]
    release: bool,
}

fn main() {
    let args = Cli::parse();

    match &args.command {
        Commands::Make { env } => {
            println!("xtask: make D1 flash binary");
            let binutils_prefix = find_binutils_prefix();
            xtask_build_d1_flash_bt0(env);
            xtask_binary_d1_flash_bt0(binutils_prefix, env);
            xtask_finialize_d1_flash_bt0(env);
        }
        Commands::Asm { env } => {
            println!("xtask: build D1 flash ELF and view assembly");
            let binutils_prefix = find_binutils_prefix();
            xtask_build_d1_flash_bt0(env);
            xtask_dump_d1_flash_bt0(binutils_prefix, env);
        }
    }
}

const DEFAULT_TARGET: &'static str = "riscv64imac-unknown-none-elf";

fn xtask_build_d1_flash_bt0(env: &Env) {
    let cargo = env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let mut command = Command::new(cargo);
    command.current_dir(project_root().join("test-d1-flash-bt0"));
    command.arg("build");
    if env.release {
        command.arg("--release");
    }
    let status = command.status().unwrap();
    if !status.success() {
        eprintln!("xtask: cargo build failed with {}", status);
        process::exit(1);
    }
}

fn xtask_binary_d1_flash_bt0(prefix: &str, env: &Env) {
    let status = Command::new(format!("{}objcopy", prefix))
        .current_dir(dist_dir(env))
        .arg("test-d1-flash-bt0")
        .arg("--binary-architecture=riscv64")
        .arg("--strip-all")
        .args(&["-O", "binary", "test-d1-flash-bt0.bin"])
        .status()
        .unwrap();

    if !status.success() {
        println!("objcopy binary failed");
        process::exit(1);
    }
}

const EGON_HEADER_LENGTH: u64 = 0x60;

// This function does:
// 1. fill in binary length
// 2. calculate checksum of bt0 image; old checksum value must be filled as stamp value
fn xtask_finialize_d1_flash_bt0(env: &Env) {
    let path = dist_dir(env);
    let mut file = File::options().read(true).write(true).open(path.join("test-d1-flash-bt0.bin"))
        .expect("open output binary file");
    let total_length = file.metadata().unwrap().len();
    if total_length < EGON_HEADER_LENGTH {
        panic!("objcopy binary size less than eGON header length, expected >= {} but is {}", EGON_HEADER_LENGTH, total_length);
    }
    let total_length = total_length as u32;
    file.seek(SeekFrom::Start(0x10)).unwrap();
    file.write_u32::<LittleEndian>(total_length).unwrap(); // fixme: fixed endian
    file.seek(SeekFrom::Start(0x0C)).unwrap();
    let stamp = file.read_u32::<LittleEndian>().unwrap();
    if stamp != 0x5F0A6C39 {
        panic!("wrong stamp value; check your generated blob and try again")
    }
    let mut checksum: u32 = 0;
    file.seek(SeekFrom::Start(0)).unwrap();
    loop {
        match file.read_u32::<LittleEndian>() {
            Ok(val) => checksum = checksum.wrapping_add(val),
            Err(e) if e.kind() == ErrorKind::UnexpectedEof => break,
            Err(e) => panic!("io error while calculating checksum: {:?}", e)
        }
    }
    file.seek(SeekFrom::Start(0x0C)).unwrap();
    file.write_u32::<LittleEndian>(checksum).unwrap(); // fixme: fixed endian
    // for C language developers: file is automatically closed when variable is out of scope
}

fn xtask_dump_d1_flash_bt0(prefix: &str, env: &Env) {
    Command::new(format!("{}objdump", prefix))
        .current_dir(dist_dir(env))
        .arg("test-d1-flash-bt0")
        .arg("-d")
        .status()
        .unwrap();
}

fn find_binutils_prefix() -> &'static str {
    for prefix in ["rust-", "riscv64-unknown-elf-", "riscv64-linux-gnu-"] {
        let mut command = Command::new(format!("{}objcopy", prefix));
        command.arg("--version");
        command.stdout(Stdio::null());
        let status = command.status().unwrap();
        if status.success() {
            return prefix;
        }
    }
    panic!(
        "error: no binutils found, try install using:
    rustup component add llvm-tools-preview
    cargo install cargo-binutils"
    );
}

fn project_root() -> PathBuf {
    Path::new(&env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(1)
        .unwrap()
        .to_path_buf()
}

fn dist_dir(env: &Env) -> PathBuf {
    let mut path_buf = project_root().join("target").join(DEFAULT_TARGET);
    path_buf = match env.release {
        false => path_buf.join("debug"),
        true => path_buf.join("release"),
    };
    path_buf
}
