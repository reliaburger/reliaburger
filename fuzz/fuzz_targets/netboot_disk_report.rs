#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = reliaburger::fuzz::netboot_disk_report(data);
});
