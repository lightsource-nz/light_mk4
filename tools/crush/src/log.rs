//! Log lines in the C implementation's shape -- `[  ERROR] message` -- because the acceptance
//! tests, and the CMake wrapper that captures crush's output, pattern-match on exactly that.

fn emit(tag: &str, msg: &str) {
        println!("[{tag:>7}] {msg}");
}

pub fn error(msg: &str) {
        emit("ERROR", msg);
}

pub fn warn(msg: &str) {
        emit("WARN", msg);
}

pub fn info(msg: &str) {
        emit("INFO", msg);
}

pub fn debug(msg: &str) {
        emit("DEBUG", msg);
}
