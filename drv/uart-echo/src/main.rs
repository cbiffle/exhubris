//! Aggressively minimal UART echo demo for the stm32g0 nucleo board.
//!
//! This brings up USART2 on PA2/3 at 9600 baud, assuming a central clock
//! frequency of 16 MHz. It then echoes whatever it receives, forever.
//!
//! The raw sends to sys here are pretty gross.

#![no_std]
#![no_main]

use userlib as _;
use hubris_task_slots::SLOTS;
use drv_stm32g0_sys_api::{Sys, PeripheralName, Port};

const UART_CLOCK_HZ: u32 = 16_000_000;
const BAUD_RATE: u32 = 9600;

/// Counter for viewing in the debugger.
#[no_mangle]
static mut CHARS_SENT: u32 = 0;

#[export_name = "main"]
fn main() -> ! {
    let sys = Sys::from(SLOTS.sys);

    // Turn on USART2
    sys.enable_clock(PeripheralName::Usart2);

    // Initialize UART
    let uart = stm32_metapac::USART2;

    uart.brr().write(|w| {
        w.set_brr((UART_CLOCK_HZ / BAUD_RATE) as u16);
    });

    uart.cr1().write(|w| {
        w.set_rxneie(true);
        w.set_re(true);
        w.set_te(true);
        w.set_ue(true);
    });

    // Set pin A2+A3 to USART2
    for pin in [2, 3] {
        sys.set_pin_alternate_mode(Port::A, pin, 1);
    }

    loop {
        // Enable the UART's IRQ output to reach our notification bit.
        userlib::sys_enable_irq(hubris_notifications::USART_2_IRQ);
        // Block waiting for that notification bit to be set.
        userlib::sys_recv_notification(hubris_notifications::USART_2_IRQ);

        // Transfer all pending characters.
        while uart.isr().read().rxne() {
            let byte = uart.rdr().read().dr() & 0xFF;
            uart.tdr().write(|w| w.set_dr(byte));
            unsafe {
                CHARS_SENT = CHARS_SENT.wrapping_add(1);
            }
        }
    }
}
