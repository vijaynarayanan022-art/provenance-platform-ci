use std::io::Read;
use std::panic;
use std::time::Duration;

fn main() {
    panic::set_hook(Box::new(|_| {}));
    let result = panic::catch_unwind(|| {
        let watchdog = prov_capture::start_watchdog(Duration::from_millis(1900));
        let mut args = std::env::args();
        let _ = args.next();
        if args.next().as_deref() != Some("codex") {
            return;
        }
        let mut input = Vec::new();
        let _ = std::io::stdin()
            .take(16 * 1024 * 1024)
            .read_to_end(&mut input);
        prov_capture::process_json(&input);
        watchdog.finish();
    });
    let _ = result;
}
