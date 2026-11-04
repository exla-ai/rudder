//! Fast local mount/launch regression, using UID1500 as the parent stand-in.
//! This executes the real namespace workload carrier, with a tiny static test
//! payload at its fixed interpreter path. It does not qualify Root custody,
//! Python, Buck2, Rust, TypeScript, or the SDK producer handshake.
//! Explicit Linux UID1500 run (requires `cc` and static libc):
//! `scripts/test-sdk-helper-local.sh --stand-in`
//! The separate real Python/TypeScript selected execution harness is registered
//! below. Neither fixture constructs a production SDK admission owner.
use super::*;

#[path = "sdk_local_e2e/mod.rs"]
mod real_sdk;

const PAYLOAD: &str = r#"
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/capability.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/stat.h>
#include <sys/statvfs.h>
#include <sys/syscall.h>
#include <unistd.h>
static void require(int ok,const char *stage){
 if(!ok){fprintf(stderr,"LOCAL_SDK_%s errno=%d\n",stage,errno);_exit(91);}
}
static void content(const char *path,const char *expected){
 char data[64]={0};int fd=open(path,O_RDONLY|O_CLOEXEC);
 require(fd>=0,"READ_OPEN");ssize_t n=read(fd,data,sizeof(data));close(fd);
 require(n==(ssize_t)strlen(expected)&&memcmp(data,expected,n)==0,"READ_CONTENT");
}
static void readonly(const char *path){
 errno=0;int fd=open(path,O_WRONLY|O_TRUNC|O_CLOEXEC);
 if(fd>=0)close(fd);require(fd<0&&errno==EROFS,"READONLY");
}
int main(int argc,char **argv){
 require(argc==6||argc==7,"FIXED_ARGV");
 require(strcmp(argv[1],"-s")==0&&strcmp(argv[2],"-P")==0&&strcmp(argv[3],"-c")==0,"FIXED_FLAGS");
 // Like the real Python workload, reset ND0 after the interpreter exec.
 require(prctl(PR_SET_DUMPABLE,0,0,0,0)==0&&prctl(PR_GET_DUMPABLE,0,0,0,0)==0,"ND0");
 require(getpid()==1&&getuid()==0&&getgid()==0,"PID1_AND_MAPPED_ID");
 require(prctl(PR_GET_NO_NEW_PRIVS,0,0,0,0)==1,"NNP");
 struct __user_cap_header_struct header={_LINUX_CAPABILITY_VERSION_3,0};
 struct __user_cap_data_struct caps[2]={{0}};
 require(syscall(SYS_capget,&header,caps)==0,"CAPGET");
 require(!(caps[0].effective|caps[0].permitted|caps[0].inheritable|caps[1].effective|caps[1].permitted|caps[1].inheritable),"ZERO_CAPS");
 for(int cap=0;cap<64;cap++){errno=0;int n=prctl(PR_CAPBSET_READ,cap,0,0,0);require(n==0||(n==-1&&errno==EINVAL),"ZERO_BOUNDING");}
 struct statvfs root;require(statvfs("/",&root)==0&&(root.f_flag&ST_RDONLY),"ROOT_READONLY");
 content("/floor-member","original floor");readonly("/floor-member");
 content("/workspace/buck2","stand-in buck2");readonly("/workspace/buck2");
 if(argc==7){
  struct stat metadata;require(stat("/proc/1",&metadata)<0&&errno==ENOENT,"PROCPLESS_SELECTED");
  int proc=atoi(argv[6]);int status=openat(proc,"self/status",O_RDONLY|O_CLOEXEC);
  require(status>=0,"HELD_OBSERVER_PROC");close(status);
  content("/workspace/src/member","original source");readonly("/workspace/src/member");
  errno=0;int denied=open("/workspace/forbidden",O_CREAT|O_WRONLY,0600);
  if(denied>=0)close(denied);require(denied<0&&errno==EROFS,"WORKSPACE_READONLY");
  int result=open("/workspace/out/result",O_WRONLY|O_CREAT|O_EXCL,0600);
  require(result>=0&&write(result,"actual output",13)==13&&fsync(result)==0&&close(result)==0,"OUTPUT_WRITE");
 }else{
  struct stat metadata;require(stat("/proc/1/status",&metadata)==0,"PID1_PROC");
 }
 puts("{\"kind\":\"local-sdk-namespace-stand-in\",\"actualPidOne\":true,\"nd0\":true,\"sdkQualified\":false}");
 return 0;
}
"#;

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        assert_eq!(unsafe { libc::getuid() }, 1500, "local stand-in requires UID1500");
        assert_eq!(unsafe { libc::getgid() }, 1500, "local stand-in requires GID1500");
        let root = PathBuf::from(format!("/workspace/rudder-sdk-local-{}", uuid::Uuid::new_v4()));
        fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
        let fixture = Self(root);
        for name in ["floor/prepared/tools/python/bin", "floor/prepared/workspace", "floor/prepared/tmp", "floor/prepared/proc", "work", "source", "output"] {
            fs::create_dir_all(fixture.0.join(name)).unwrap();
        }
        fs::write(fixture.0.join("floor/prepared/floor-member"), b"original floor").unwrap();
        fs::write(fixture.0.join("floor/buck2"), b"stand-in buck2").unwrap();
        fs::write(fixture.0.join("source/member"), b"original source").unwrap();
        fs::write(fixture.0.join("payload.c"), PAYLOAD).unwrap();
        let status = Command::new("cc").args(["-static", "-O2", "-Wall", "-Werror", "-Wno-misleading-indentation"])
            .arg(fixture.0.join("payload.c")).arg("-o").arg(fixture.0.join("floor/prepared/tools/python/bin/python3"))
            .status().expect("local fixture needs a C compiler with static libc");
        assert!(status.success(), "local stand-in fixture compilation failed");
        fixture
    }
}
impl Drop for Fixture { fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); } }

fn directory(path: &Path) -> Result<OwnedFd> {
    Ok(OpenOptions::new().read(true).custom_flags(libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC).open(path)?.into())
}

fn exercise(selected: bool) {
    let fixture = Fixture::new();
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0, "local helper test fork failed");
    if pid == 0 {
        let outcome = (|| -> Result<()> {
            ensure!(unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) } == 0, "local ND0 unavailable");
            ensure!(unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } == 0, "local NNP unavailable");
            let userns = WorkloadUserNamespace::fork()?;
            let floor = directory(&fixture.0.join("floor"))?;
            let workspace = directory(&fixture.0.join("work"))?;
            ensure!(unsafe { libc::fchdir(workspace.as_raw_fd()) } == 0, "local scratch chdir failed");
            let source = directory(&fixture.0.join("source"))?;
            let output = directory(&fixture.0.join("output"))?;
            let raw = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
            ensure!(raw >= 0, "local original cancellation unavailable");
            let cancel = unsafe { OwnedFd::from_raw_fd(raw) };
            let request = serde_json::json!({"kind":"local-test-data-only"});
            let original = if selected { FinalizationBudget::begin(Duration::from_secs(10))? }
                else { FinalizationBudget::begin_sdk_qualification()? };
            let constraint = original.constraint();
            validate_sdk_process_clock(&constraint, !selected)?;
            let original = if selected { FinalizationBudget::from_constraint(&constraint)? }
                else { FinalizationBudget::from_sdk_qualification_constraint(&constraint)? };
            ensure!(original.constraint() == constraint, "local original SDK constraint changed");
            let mut stderr = None;
            let result = run_namespace_retaining(userns, &floor, &workspace, &original,
                selected.then_some((&request, &source, &output, &cancel)), &mut stderr);
            // Only fixed fixture diagnostics, never user source or SDK logs.
            if result.is_err() { if let Some(file) = stderr { let mut bytes = [0;512]; if let Ok(n) = file.read_at(&mut bytes,0) { eprintln!("local stand-in: {}",String::from_utf8_lossy(&bytes[..n])); } } }
            let value = result?;
            ensure!(value["kind"] == "local-sdk-namespace-stand-in" && value["actualPidOne"] == true && value["nd0"] == true && value["sdkQualified"] == false, "local stand-in result differs");
            ensure!(unsafe { libc::prctl(libc::PR_GET_DUMPABLE, 0, 0, 0, 0) } == 0, "helper relaxed ND0");
            Ok(())
        })();
        let code = match outcome { Ok(()) => 0, Err(error) => { eprintln!("local actual helper: {error:#}"); 1 } };
        unsafe { libc::_exit(code) }
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let mut status = 0;
    loop {
        let observed = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        if observed == pid { break; }
        assert!(observed == 0 || io::Error::last_os_error().kind() == io::ErrorKind::Interrupted, "original local child wait failed");
        if std::time::Instant::now() >= deadline {
            unsafe { libc::kill(pid, libc::SIGKILL); libc::waitpid(pid, &mut status, 0); }
            panic!("original local helper exceeded its test bound");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0, "local helper scenario failed: {status}");
    assert_eq!(fs::read(fixture.0.join("floor/prepared/floor-member")).unwrap(), b"original floor");
    assert_eq!(fs::read(fixture.0.join("source/member")).unwrap(), b"original source");
    if selected { assert_eq!(fs::read(fixture.0.join("output/result")).unwrap(), b"actual output"); }
    else { assert!(fs::read_dir(fixture.0.join("output")).unwrap().next().is_none()); }
}

#[test]
#[ignore = "local Linux UID1500 stand-in; requires cc, static libc and enabled user namespaces"]
fn local_uid1500_runs_actual_fixed_namespace_carrier_with_stand_in_payload() { exercise(false); }

#[test]
#[ignore = "local Linux UID1500 stand-in; requires cc, static libc and enabled user namespaces"]
fn local_uid1500_runs_actual_selected_namespace_carrier_with_retained_source_and_output() { exercise(true); }
