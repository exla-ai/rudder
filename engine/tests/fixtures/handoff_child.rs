//! Fresh child for the private one-use handoff protocol's Linux tests.
#[cfg(target_os = "linux")]
fn controller_pidfds(parent: u32) -> Vec<i32> {
    let mut descriptors = Vec::new();
    for entry in std::fs::read_dir("/proc/self/fdinfo").unwrap() {
        let entry = entry.unwrap();
        let contents = match std::fs::read_to_string(entry.path()) {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => panic!("owned fdinfo read failed: {error}"),
        };
        if contents.lines().any(|line| {
            line.strip_prefix("Pid:\t")
                .is_some_and(|pid| pid.parse::<u32>().ok() == Some(parent))
        }) {
            descriptors.push(entry.file_name().to_str().unwrap().parse().unwrap());
        }
    }
    descriptors
}

#[cfg(target_os = "linux")]
fn main() {
    let args = std::env::args().collect::<Vec<_>>();
    if args.get(1).is_some_and(|mode| mode == "controller-fd-exec") {
        use std::os::unix::fs::FileTypeExt;
        let parent = args[2].parse().unwrap();
        let control: i32 = args[3].parse().unwrap();
        assert!(
            unsafe { libc::fcntl(control, libc::F_GETFD) } >= 0,
            "the deliberately inherited non-CLOEXEC control must survive"
        );
        assert!(std::fs::metadata(format!("/proc/self/fd/{control}"))
            .unwrap()
            .file_type()
            .is_char_device());
        assert!(
            controller_pidfds(parent).is_empty(),
            "fresh exec inherited the authenticated controller pidfd"
        );
        println!("controller-fd-exec-ok");
        return;
    }
    let managed = args.get(3).is_some_and(|mode| mode == "managed-controller");
    let spec = if managed {
        use std::{
            os::{
                linux::net::SocketAddrExt,
                unix::net::{SocketAddr, UnixStream},
            },
            sync::Arc,
        };
        let (spec, control) = rudder_engine::execution::handoff::receive_managed(
            &args[1],
            args[2].parse().unwrap(),
        )
        .unwrap();
        let controller = control.controller_process();
        let retained = control.controller_process();
        assert!(Arc::ptr_eq(&controller, &retained));
        drop(control);
        drop(controller);
        retained.check_current().unwrap();
        let (wrong_peer, _other) = UnixStream::pair().unwrap();
        assert!(
            retained.verify_peer(&wrong_peer).is_err(),
            "same UID/helper PID must not replace the authenticated parent"
        );
        let parent = UnixStream::connect_addr(
            &SocketAddr::from_abstract_name(args[4].as_bytes()).unwrap(),
        )
        .unwrap();
        retained.verify_peer(&parent).unwrap();
        retained.check_current().unwrap();
        assert_eq!(
            controller_pidfds(args[2].parse().unwrap()).len(),
            1,
            "the retained witness must still own its actual controller pidfd"
        );
        // Test the held witness across a real exec, with a non-CLOEXEC FD as a
        // positive detector control. This is not the restricted guest path.
        use std::os::fd::AsRawFd;
        let control = std::fs::File::open("/dev/null").unwrap();
        let control_fd = control.as_raw_fd();
        assert_eq!(unsafe { libc::fcntl(control_fd, libc::F_SETFD, 0) }, 0);
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "controller-fd-exec",
                args[2].as_str(),
                control_fd.to_string().as_str(),
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"controller-fd-exec-ok\n");
        retained.check_current().unwrap();
        spec
    } else {
        std::thread::sleep(std::time::Duration::from_millis(150));
        rudder_engine::execution::handoff::receive(&args[1], args[2].parse().unwrap()).unwrap()
    };
    let fixture = b"known-fixture-secret-never-argv";
    assert_eq!(spec.environment["CANARY_SECRET"].as_bytes(), fixture);
    for path in ["/proc/self/cmdline", "/proc/self/environ"] {
        assert!(!std::fs::read(path)
            .unwrap()
            .windows(fixture.len())
            .any(|bytes| bytes == fixture));
    }
    println!(
        "{}",
        if managed {
            "managed-controller-witness-ok"
        } else {
            "handoff-ok"
        }
    );
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("execution handoff test requires Linux");
    std::process::exit(1);
}
