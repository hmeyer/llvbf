#![no_std]
#![no_main]

extern "C" {
    fn __llvbf_putc(c: i32);
    fn __llvbf_exit();
}

fn putc(c: u8) {
    unsafe { __llvbf_putc(c as i32) }
}

fn put_str(s: &str) {
    for b in s.bytes() {
        putc(b);
    }
}

fn put_num(mut n: u32) {
    let mut buf = [0u8; 10];
    let mut i = buf.len();
    if n == 0 {
        putc(b'0');
        return;
    }
    while n > 0 {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    for &b in &buf[i..] {
        putc(b);
    }
}

fn fib(n: u32) -> u32 {
    if n < 2 { n } else { fib(n - 1) + fib(n - 2) }
}

#[no_mangle]
pub extern "C" fn main() -> i32 {
    put_str("Rust says hello from Brainfuck\n");
    let mut primes = [true; 50];
    let mut i = 2;
    while i < 50 {
        if primes[i] {
            let mut j = i * i;
            while j < 50 {
                primes[j] = false;
                j += i;
            }
        }
        i += 1;
    }
    for (k, &p) in primes.iter().enumerate().skip(2) {
        if p {
            put_num(k as u32);
            putc(b' ');
        }
    }
    putc(b'\n');
    for n in 0..12 {
        put_str("fib(");
        put_num(n);
        put_str(") = ");
        put_num(fib(n));
        putc(b'\n');
    }
    0
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    put_str("panic\n");
    unsafe { __llvbf_exit() }
    loop {}
}
