use super::*;
#[test]
fn only_exact_new_public_image_config_files_are_runtime_sources(){
    for path in ["/etc/ssl/openssl.cnf","/etc/python3.11/sitecustomize.py"]{assert!(public_runtime(Path::new(path)),"{path}");}
    for path in ["/etc/ssl/private/key","/etc/ssl/openssl.cnf.backup","/etc/ssl/openssl.cnf/child","/etc/python3.11/other.py","/etc/python3.11/sitecustomize.py.extra","/etc/python3.11/sitecustomize.py/child","/etc/python3.12/sitecustomize.py","/etc/shadow"]{assert!(!public_runtime(Path::new(path)),"{path}");}
}
