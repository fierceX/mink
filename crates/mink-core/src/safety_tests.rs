use super::*;

#[test]
fn blocks_sudo() {
    assert!(deny_bash_command_reason("sudo rm /tmp/foo").is_some());
}

#[test]
fn blocks_rm_rf_root() {
    assert!(deny_bash_command_reason("rm -rf /").is_some());
    assert!(deny_bash_command_reason("rm -fr /").is_some());
    assert!(deny_bash_command_reason("rm -rf /*").is_some());
    assert!(deny_bash_command_reason("rm -rf \"/\"").is_some());
}

#[test]
fn blocks_rm_rf_absolute_paths_outside_temp() {
    assert!(deny_bash_command_reason("rm -rf /usr/local").is_some());
    assert!(deny_bash_command_reason("rm -rf /tmp/a /usr/b").is_some());
    // 词法折叠后落回非临时目录：不得借 /tmp 前缀穿越。
    assert!(deny_bash_command_reason("rm -rf /tmp/../etc").is_some());
    assert!(deny_bash_command_reason("rm -fr /tmp/../etc").is_some());
    assert!(deny_bash_command_reason("rm -rf /tmp/a/../../etc").is_some());
    assert!(deny_bash_command_reason("rm -rf /private/tmp/../etc").is_some());
    assert!(deny_bash_command_reason("rm -rf /../etc").is_some());
    // 前缀不得越界：/tmpfoo 不是 /tmp（组件级比较）。
    assert!(deny_bash_command_reason("rm -rf /tmpfoo").is_some());
}

#[test]
fn blocks_rm_rf_unresolvable_targets() {
    // 命令替换/变量/字符类/花括号展开：静态不可判定 → 保守拦截。
    assert!(deny_bash_command_reason("rm -rf /tmp/$(date +%s)").is_some());
    assert!(deny_bash_command_reason("rm -rf /tmp/$DIR/x").is_some());
    assert!(deny_bash_command_reason("rm -rf /tmp/[ab]/x").is_some());
    assert!(deny_bash_command_reason("rm -rf /tmp/{a,b}").is_some());
}

#[test]
fn blocks_rm_rf_escaped_and_quoted_traversal() {
    // `\/` 在 shell 里就是 `/`，真实目标是 /etc。
    assert!(deny_bash_command_reason(r"rm -rf /tmp/\../etc").is_some());
    // 引号内空格让词跨越 `..`，真实目标同样是 /etc。
    assert!(deny_bash_command_reason("rm -rf \"/tmp/a b/../../etc\"").is_some());
    // 未闭合引号：词边界不可知。
    assert!(deny_bash_command_reason("rm -rf \"/tmp/x").is_some());
}

#[test]
fn allows_quoted_temp_paths_with_literal_apostrophe() {
    // 受限词法扫描替代黑名单后，合法引号写法不再被误伤。
    assert!(deny_bash_command_reason("rm -rf \"/tmp/it's/x\"").is_none());
    assert!(deny_bash_command_reason("rm -rf '/tmp/it is fine'").is_none());
    assert!(deny_bash_command_reason("rm -rf /tmp/a\\ b").is_none());
}

#[test]
fn allows_rm_rf_temp_paths() {
    assert!(deny_bash_command_reason("rm -rf /tmp/rawtest").is_none());
    assert!(deny_bash_command_reason("rm -rf /tmp/rawtest && mkdir -p /tmp/rawtest").is_none());
    assert!(deny_bash_command_reason("rm -rf \"/tmp/photos-demo\"").is_none());
    // 词法等价写法同样放行（折叠后仍在临时目录内）。
    assert!(deny_bash_command_reason("rm -rf /tmp/a/../b").is_none());
    assert!(deny_bash_command_reason("rm -rf /tmp//x").is_none());
    assert!(deny_bash_command_reason("rm -rf /tmp/./y").is_none());
    assert!(deny_bash_command_reason("rm -rf /tmp/*").is_none());
    assert!(deny_bash_command_reason("rm -fr /tmp/rawtest").is_none());
    assert!(deny_bash_command_reason("rm -rf /private/tmp/x").is_none());
    assert!(deny_bash_command_reason("rm -rf /var/folders/ab/cd").is_none());
    assert!(deny_bash_command_reason("rm -rf /tmp").is_none());
}

#[test]
fn blocks_shutdown() {
    assert!(deny_bash_command_reason("shutdown now").is_some());
}

#[test]
fn blocks_find_delete() {
    assert!(deny_bash_command_reason("find . -name '*.tmp' -delete").is_some());
}

#[test]
fn allows_safe_commands() {
    assert!(deny_bash_command_reason("echo hello").is_none());
    assert!(deny_bash_command_reason("ls -la").is_none());
    assert!(deny_bash_command_reason("cat /tmp/file").is_none());
}

#[test]
fn allows_dev_null_redirection() {
    assert!(deny_bash_command_reason("echo harmless >/dev/null").is_none());
}

#[test]
fn blocks_empty_command() {
    assert!(deny_bash_command_reason("").is_some());
    assert!(deny_bash_command_reason("   ").is_some());
}

#[cfg(unix)]
#[test]
fn blocks_rm_rf_symlink_escape_and_allows_temp_symlink() {
    let dir = std::env::temp_dir().join(format!("mink-safety-symlink-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let escape = dir.join("escape");
    std::os::unix::fs::symlink("/etc", &escape).unwrap();
    let command = format!("rm -rf {}", escape.display());
    assert!(
        deny_bash_command_reason(&command).is_some(),
        "symlink escape must be blocked: {command}"
    );

    let benign = dir.join("benign");
    std::os::unix::fs::symlink("/tmp", &benign).unwrap();
    let command = format!("rm -rf {}", benign.display());
    assert!(
        deny_bash_command_reason(&command).is_none(),
        "symlink into temp must stay allowed: {command}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn glob_plus_parent_traversal_matches_real_semantics() {
    // `/tmp/*/../etc` 对每个展开项都回到 /tmp，真实目标 /tmp/etc（放行）；
    // `/tmp/*/../../etc` 真实目标为 /etc（拦截）。
    assert!(deny_bash_command_reason("rm -rf /tmp/*/../etc").is_none());
    assert!(deny_bash_command_reason("rm -rf /tmp/*/../../etc").is_some());
}

#[cfg(unix)]
#[test]
fn resolve_real_path_follows_relative_symlink_from_parent_dir() {
    // 相对链接目标必须以链接自身的目录为基准解析（`../sibling`），
    // 而不是以根为基准。
    let dir = std::env::temp_dir().join(format!("mink-safety-rel-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("inner")).unwrap();
    std::fs::create_dir_all(dir.join("sibling")).unwrap();
    std::os::unix::fs::symlink("../sibling", dir.join("inner/rel")).unwrap();

    let real_dir = dir.canonicalize().unwrap();
    let resolved = resolve_real_path(&format!("{}/inner/rel/leaf", dir.display())).unwrap();
    assert_eq!(
        resolved,
        real_dir.join("sibling").join("leaf"),
        "relative symlink must resolve from its own directory"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn resolve_real_path_rejects_symlink_loops() {
    let dir = std::env::temp_dir().join(format!("mink-safety-loop-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::os::unix::fs::symlink("loop_b", dir.join("loop_a")).unwrap();
    std::os::unix::fs::symlink("loop_a", dir.join("loop_b")).unwrap();

    let command = format!("rm -rf {}/loop_a", dir.display());
    assert!(
        deny_bash_command_reason(&command).is_some(),
        "cyclic symlinks must fail closed: {command}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn shell_word_split_matches_posix_sh_oracle() {
    // 以 `sh -c 'printf %s\0 …'` 为真值：受限扫描器与 POSIX 词法在可确定
    // 用例上必须逐词一致（含转义、单/双引号与内嵌空格）。
    let cases = [
        "/tmp/plain",
        "/tmp/a\\ b",
        "/tmp/a\\/b",
        "\"/tmp/quoted space\"/x",
        "'/tmp/single quoted'/y",
        "/tmp/one /tmp/two",
    ];
    for case in cases {
        let words = split_shell_words(case).expect("statically resolvable");
        let output = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("printf '%s\\0' {case}"))
            .output()
            .expect("sh must run");
        assert!(output.status.success(), "sh failed for case: {case}");
        let oracle: Vec<String> = output
            .stdout
            .split(|byte| *byte == 0)
            .filter(|part| !part.is_empty())
            .map(|part| String::from_utf8_lossy(part).into_owned())
            .collect();
        assert_eq!(words, oracle, "scanner vs sh mismatch for case: {case}");
    }
}
