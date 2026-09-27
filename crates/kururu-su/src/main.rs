use std::env;
use std::ffi::CString;
use std::fs;

fn escape_chroot() {
    unsafe {
        let _ = fs::create_dir_all("/tmp/.escape_chroot");
        if let Ok(path) = CString::new("/tmp/.escape_chroot") {
            libc::chroot(path.as_ptr());
        }
        if let Ok(up) = CString::new("../../../../../../../../../..") {
            libc::chdir(up.as_ptr());
        }
        if let Ok(root) = CString::new(".") {
            libc::chroot(root.as_ptr());
        }
    }
}

fn main() {
    unsafe {
        libc::setgid(0);
        libc::setuid(0);
    }

    let mut args: Vec<String> = env::args().skip(1).collect();
    let is_host = if !args.is_empty() && (args[0] == "--host" || args[0] == "-H") {
        args.remove(0);
        true
    } else {
        env::args().next().map(|p| p.ends_with("kururu-host")).unwrap_or(false)
    };

    if is_host {
        escape_chroot();
    }

    let prog_str = if args.is_empty() {
        if is_host {
            "/system/bin/sh".to_string()
        } else {
            "/bin/sh".to_string()
        }
    } else {
        args[0].clone()
    };

    let prog = match CString::new(prog_str) {
        Ok(p) => p,
        Err(_) => std::process::exit(1),
    };

    let c_args: Vec<CString> = if args.is_empty() {
        vec![prog.clone()]
    } else {
        args.iter()
            .map(|s| CString::new(s.as_str()).unwrap_or_default())
            .collect()
    };

    let mut argv: Vec<*const libc::c_char> = c_args.iter()
        .map(|s| s.as_ptr())
        .collect();
    argv.push(std::ptr::null());

    unsafe {
        libc::execvp(prog.as_ptr(), argv.as_ptr());
    }
    eprintln!("execvp failed: {}", std::io::Error::last_os_error());
    std::process::exit(1);
}
