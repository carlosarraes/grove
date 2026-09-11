use grove::health::Memory;

/// Both platforms are parsed from the text their tools print, so the parser is what gets
/// tested; the tools themselves are not available on the other platform.
#[test]
fn macos_memory_is_read_from_memory_pressure_and_swapusage() {
    let pressure = "The system has 25769803776 (6291456 pages with a page size of 4096).\n\nStats: \n...\nSystem-wide memory free percentage: 71%\n";
    let swap = "vm.swapusage: total = 15360.00M  used = 5120.00M  free = 10240.00M  (encrypted)\n";
    let m = Memory::from_macos(pressure, swap).expect("parse");
    assert_eq!(m.free_percent, 71);
    assert_eq!(m.swap_used, 5120 << 20);
    assert_eq!(m.swap_total, 15360 << 20);
}

#[test]
fn linux_memory_is_read_from_meminfo() {
    let meminfo = "MemTotal:       32000000 kB\nMemFree:         1000000 kB\nMemAvailable:    8000000 kB\nSwapTotal:       4000000 kB\nSwapFree:        1000000 kB\n";
    let m = Memory::from_meminfo(meminfo).expect("parse");
    assert_eq!(m.free_percent, 25);
    assert_eq!(m.swap_used, 3_000_000 * 1024);
    assert_eq!(m.swap_total, 4_000_000 * 1024);
}

/// Swap at its ceiling is the incident signal; free pages are not.
#[test]
fn swap_near_its_ceiling_is_a_failure_whatever_free_pages_say() {
    let m = Memory {
        free_percent: 40,
        swap_used: 15 << 30,
        swap_total: 15 << 30,
    };
    assert!(m.exhausted());
    let ok = Memory {
        free_percent: 3,
        swap_used: 1 << 30,
        swap_total: 15 << 30,
    };
    assert!(
        !ok.exhausted(),
        "low free pages alone are how macOS always looks"
    );
    let none = Memory {
        free_percent: 30,
        swap_used: 0,
        swap_total: 0,
    };
    assert!(!none.exhausted());
}
