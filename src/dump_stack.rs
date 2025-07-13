// This will show C frames in the backtrace
#[no_mangle]
pub extern "C" fn dump_stack() {
    let bt = std::backtrace::Backtrace::force_capture();
    println!("{}", bt);
}
