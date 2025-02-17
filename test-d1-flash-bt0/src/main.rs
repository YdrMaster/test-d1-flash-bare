#![feature(naked_functions, asm_sym, asm_const)]
#![feature(default_alloc_error_handler)]
#![no_std]
#![no_main]

use core::{arch::asm, panic::PanicInfo};
use d1_pac::Peripherals;
use embedded_hal::digital::blocking::OutputPin;

#[macro_use]
mod logging;
mod ccu;
mod flash;
mod gpio;
mod jtag;
mod mctl;
mod spi;
mod time;
mod uart;

use ccu::Clocks;
#[cfg(feature = "nand")]
use flash::SpiNand;
#[cfg(feature = "nor")]
use flash::SpiNor;
use gpio::Gpio;
use jtag::Jtag;
use mctl::RAM_BASE;
use spi::Spi;
use time::U32Ext;
use uart::{Config, Parity, Serial, StopBits, WordLength};

// taken from oreboot
pub type EntryPoint = unsafe extern "C" fn(r0: usize, r1: usize);

const STACK_SIZE: usize = 1 * 1024; // 1KiB

#[link_section = ".bss.uninit"]
static mut SBI_STACK: [u8; STACK_SIZE] = [0; STACK_SIZE];

/// Jump over head data to executable code.
///
/// # Safety
///
/// Naked function.
#[naked]
#[link_section = ".head.text"]
#[export_name = "head_jump"]
pub unsafe extern "C" fn head_jump() {
    asm!(
        ".option push",
        ".option rvc",
        "c.j    0x60",
        ".option pop",
        // sym start,
        options(noreturn)
    )
}

// todo: option(noreturn) generates an extra `unimp` insn

#[repr(C)]
pub struct HeadData {
    magic: [u8; 8],
    checksum: u32,
    length: u32,
    pub_head_size: u32,
    fel_script_address: u32,
    fel_uenv_length: u32,
    dt_name_offset: u32,
    dram_size: u32,
    boot_media: u32,
    string_pool: [u32; 13],
}

const STAMP_CHECKSUM: u32 = 0x5F0A6C39;

// clobber used by KEEP(*(.head.data)) in link script
#[link_section = ".head.data"]
pub static HEAD_DATA: HeadData = HeadData {
    magic: *b"eGON.BT0",
    checksum: STAMP_CHECKSUM, // real checksum filled by blob generator
    length: 0,                // real size filled by blob generator
    pub_head_size: 0,
    fel_script_address: 0,
    fel_uenv_length: 0,
    dt_name_offset: 0,
    dram_size: 0,
    boot_media: 0,
    string_pool: [0; 13],
};

/// Jump over head data to executable code.
///
/// # Safety
///
/// Naked function.
#[naked]
#[export_name = "start"]
#[link_section = ".text.entry"]
pub unsafe extern "C" fn start() -> ! {
    asm!(
        // 1. clear cache and processor states
        "csrw   mie, zero",
        "li     t2, 0x30013",
        "csrs   0x7c2, t2", // MCOR
        // 2. initialize programming langauge runtime
        // clear bss segment
        "la     t0, sbss",
        "la     t1, ebss",
        "1:",
        "bgeu   t0, t1, 1f",
        "sd     x0, 0(t0)",
        "addi   t0, t0, 4",
        "j      1b",
        "1:",
        // does not init data segment as BT0 runs in sram
        // 3. prepare stack
        "la     sp, {stack}",
        "li     t0, {stack_size}",
        "add    sp, sp, t0",
        "la     a0, {head_data}",
        "j      {main}",
        // function `main` returns address of next stage,
        // it drops all peripherals it holds when goes out of scope
        // now, jump to dram code
        "j      {finish}",
        stack      =   sym SBI_STACK,
        stack_size = const STACK_SIZE,
        head_data  =   sym HEAD_DATA,
        main       =   sym main,
        finish     =   sym finish,
        options(noreturn)
    )
}

extern "C" fn main() -> usize {
    // there was configure_ccu_clocks, but ROM code have already done configuring for us
    let p = Peripherals::take().unwrap();
    // rom provided clock frequency, it's hard coded in bt0 stage
    let clocks = Clocks {
        psi: 600_000_000.hz(),
        apb1: 24_000_000.hz(),
    };
    let gpio = Gpio::new(p.GPIO);

    // configure jtag interface
    let tms = gpio.portf.pf0.into_function_4();
    let tck = gpio.portf.pf5.into_function_4();
    let tdi = gpio.portf.pf1.into_function_4();
    let tdo = gpio.portf.pf3.into_function_4();
    let _jtag = Jtag::new((tms, tck, tdi, tdo));

    // light up led
    let mut pb5 = gpio.portb.pb5.into_output();
    pb5.set_high().unwrap();
    let mut pc1 = gpio.portc.pc1.into_output();
    pc1.set_high().unwrap();

    // prepare serial port logger
    let tx = gpio.portb.pb8.into_function_6();
    let rx = gpio.portb.pb9.into_function_6();
    let config = Config {
        baudrate: 115200.bps(),
        wordlength: WordLength::Eight,
        parity: Parity::None,
        stopbits: StopBits::One,
    };
    let serial = Serial::new(p.UART0, (tx, rx), config, &clocks);
    crate::logging::set_logger(serial);

    println!("oreboot 🦀").ok();

    let ram_size = mctl::init();
    println!("{}M 🐏", ram_size).ok();

    // prepare spi interface to use in flash
    let sck = gpio.portc.pc2.into_function_2();
    let scs = gpio.portc.pc3.into_function_2();
    let mosi = gpio.portc.pc4.into_function_2();
    let miso = gpio.portc.pc5.into_function_2();
    let spi = Spi::new(
        p.SPI0,
        (sck, scs, mosi, miso),
        spi::MODE_3,
        100_000_000.hz(),
        &clocks,
    );

    #[cfg(feature = "nor")]
    {
        use core::ptr::{read_volatile, write_volatile};
        let mut flash = SpiNor::new(spi);

        // e.g., GigaDevice (GD) is 0xC8 and GD25Q128 is 0x4018
        // see flashrom/flashchips.h for details and more
        let id = flash.read_id();
        println!("SPI flash vendor {} part {}{}", id[0], id[1], id[2],).ok();
        println!().ok();

        // 32K, the size of boot0
        let base = 0x1 << 15;
        let size: usize = 15400;
        for i in 0..size {
            let off = base + i * 4;
            let buf = flash.copy_into([(off >> 16) as u8, (off >> 8) as u8 % 255, off as u8 % 255]);

            let addr = RAM_BASE + i * 4;
            let val = u32::from_le_bytes([buf[3], buf[2], buf[1], buf[0]]);
            unsafe { write_volatile(addr as *mut u32, val) };
            let rval = unsafe { read_volatile(addr as *mut u32) };

            if rval != val {
                println!("MISMATCH {} r{} :: {}", addr, rval, val).ok();
            }
        }
        let _ = flash.free().free();
    }

    #[cfg(feature = "nand")]
    {
        let mut flash = SpiNand::new(spi);
        println!("Oreboot read flash ID = {:?}", flash.read_id()).ok();
        let mut page = [0u8; 256];
        flash.copy_into(0, &mut page);
        let _ = flash.free().free();
    }

    for _ in 0..1000_0000 {
        core::hint::spin_loop();
    }
    println!("Run payload at {}", RAM_BASE).ok();

    // returns an address of dram payload; now cpu would jump to this address
    // and run code inside
    RAM_BASE
}

// jump to dram
extern "C" fn finish(address: extern "C" fn()) -> ! {
    unsafe { asm!("jr {}", in(reg) address) }
    loop {
        unsafe { asm!("wfi") }
    }
}

#[cfg_attr(not(test), panic_handler)]
fn panic(info: &PanicInfo) -> ! {
    if let Some(location) = info.location() {
        println!(
            "panic occurred in file '{}' at line {}",
            location.file(),
            location.line(),
        )
        .ok();
    } else {
        println!("panic occurred but can't get location information...").ok();
    };
    loop {
        core::hint::spin_loop();
    }
}
