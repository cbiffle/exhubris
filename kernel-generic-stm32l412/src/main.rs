#![no_std]
#![no_main]

extern crate stm32l4;

use cortex_m_rt::entry;

#[entry]
fn main() -> ! {
    const CYCLES_PER_MS: u32 = 4_000;

    unsafe { hubris_kern::startup::start_kernel(CYCLES_PER_MS) }
}