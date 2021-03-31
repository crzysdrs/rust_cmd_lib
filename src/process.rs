use crate::{builtin_true, CmdResult, FunResult};
use faccess::{AccessMode, PathExt};
use lazy_static::lazy_static;
use log::{debug, error, info};
use std::collections::HashMap;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Error, ErrorKind, Read, Result, Write};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Mutex;

/// Process environment for builtin or custom commands
pub struct CmdEnv<'a> {
    inbuf: Vec<u8>,
    outbuf: Vec<u8>,
    errbuf: Vec<u8>,
    args: &'a [String],
    vars: &'a HashMap<String, String>,
    current_dir: &'a str,
}
impl<'a> CmdEnv<'a> {
    fn new(args: &'a [String], vars: &'a HashMap<String, String>, current_dir: &'a str) -> Self {
        CmdEnv {
            inbuf: vec![],
            outbuf: vec![],
            errbuf: vec![],
            args,
            vars,
            current_dir,
        }
    }

    pub fn args(&self) -> &[String] {
        self.args
    }

    pub fn var(&self, key: &str) -> Option<&String> {
        self.vars.get(key)
    }

    pub fn current_dir(&self) -> &str {
        self.current_dir
    }

    pub fn stdin(&self) -> impl Read + '_ {
        self.inbuf.as_slice()
    }

    pub fn stdout(&mut self) -> impl Write + '_ {
        &mut self.outbuf
    }

    pub fn stderr(&mut self) -> impl Write + '_ {
        &mut self.errbuf
    }
}

type FnFun = fn(&mut CmdEnv) -> CmdResult;

lazy_static! {
    static ref CMD_MAP: Mutex<HashMap<&'static str, FnFun>> = {
        // needs explicit type, or it won't compile
        let mut m: HashMap<&'static str, FnFun> = HashMap::new();
        m.insert("", builtin_true);
        Mutex::new(m)
    };
}

#[doc(hidden)]
pub fn export_cmd(cmd: &'static str, func: FnFun) {
    CMD_MAP.lock().unwrap().insert(cmd, func);
}

/// set debug mode or not, false by default
///
/// Setting environment variable CMD_LIB_DEBUG=0|1 has the same effect
pub fn set_debug(enable: bool) {
    std::env::set_var("CMD_LIB_DEBUG", if enable { "1" } else { "0" });
}

/// set pipefail or not, true by default
///
/// Setting environment variable CMD_LIB_PIPEFAIL=0|1 has the same effect
pub fn set_pipefail(enable: bool) {
    std::env::set_var("CMD_LIB_PIPEFAIL", if enable { "1" } else { "0" });
}

#[doc(hidden)]
#[derive(Default)]
pub struct GroupCmds {
    group_cmds: Vec<(Cmds, Option<Cmds>)>, // (cmd, orCmd) pairs
    current_dir: String,
}

impl GroupCmds {
    pub fn add(mut self, cmds: Cmds, or_cmds: Option<Cmds>) -> Self {
        self.group_cmds.push((cmds, or_cmds));
        self
    }

    pub fn run_cmd(&mut self) -> CmdResult {
        for cmds in self.group_cmds.iter_mut() {
            if let Err(err) = cmds.0.run_cmd(&mut self.current_dir) {
                if let Some(or_cmds) = &mut cmds.1 {
                    let ret = or_cmds.run_cmd(&mut self.current_dir);
                    if let Err(err) = ret {
                        error!("Running {} failed, Error: {}", or_cmds.get_full_cmds(), err);
                        return Err(err);
                    }
                } else {
                    error!("Running {} failed, Error: {}", cmds.0.get_full_cmds(), err);
                    return Err(err);
                }
            }
        }
        Ok(())
    }

    pub fn run_fun(&mut self) -> FunResult {
        let mut last_cmd = self.group_cmds.pop().unwrap();
        self.run_cmd()?;
        // run last function command
        let ret = last_cmd.0.run_fun(&mut self.current_dir);
        if let Err(e) = ret {
            if let Some(or_cmds) = &mut last_cmd.1 {
                let or_ret = or_cmds.run_fun(&mut self.current_dir);
                if let Err(ref err) = or_ret {
                    error!("Running {} failed, Error: {}", or_cmds.get_full_cmds(), err);
                }
                or_ret
            } else {
                let full_cmds = last_cmd.0.get_full_cmds();
                error!("Running {} failed, Error: {}", full_cmds, e);
                Err(e)
            }
        } else {
            ret
        }
    }

    pub fn spawn(mut self) -> Result<WaitCmd> {
        assert_eq!(self.group_cmds.len(), 1);
        let mut cmds = self.group_cmds.pop().unwrap().0;
        let ret = cmds.spawn(&mut self.current_dir, false);
        if let Err(ref err) = ret {
            error!("Spawning {} failed, Error: {}", cmds.get_full_cmds(), err);
        }
        ret
    }

    pub fn spawn_with_output(mut self) -> Result<WaitFun> {
        assert_eq!(self.group_cmds.len(), 1);
        let mut cmds = self.group_cmds.pop().unwrap().0;
        match cmds.spawn(&mut self.current_dir, true) {
            Ok(ret) => Ok(WaitFun(ret.0)),
            Err(err) => {
                error!("Spawning {} failed, Error: {}", cmds.get_full_cmds(), err);
                Err(err)
            }
        }
    }
}

#[doc(hidden)]
#[derive(Default)]
pub struct Cmds {
    cmds: Vec<Cmd>,
}

impl Cmds {
    pub fn pipe(mut self, cmd: Cmd) -> Self {
        self.cmds.push(cmd.gen_command());
        self
    }

    fn get_full_cmds(&self) -> String {
        self.cmds
            .iter()
            .map(|cmd| cmd.debug_str())
            .collect::<Vec<_>>()
            .join(" | ")
    }

    fn spawn(&mut self, current_dir: &mut String, with_output: bool) -> Result<WaitCmd> {
        if std::env::var("CMD_LIB_DEBUG") == Ok("1".into()) {
            debug!("Running {} ...", self.get_full_cmds());
        }

        // set up redirects
        for cmd in self.cmds.iter_mut() {
            cmd.setup_redirects()?;
        }

        // spawning all the sub-processes
        let mut children: Vec<(ProcHandle, String)> = Vec::new();
        let len = self.cmds.len();
        let mut last_child = None;
        for (i, cmd) in self.cmds.iter_mut().enumerate() {
            let child = cmd.spawn(
                current_dir,
                with_output,
                i == 0,
                i == len - 1,
                &mut last_child,
            )?;
            children.push(child);
            last_child = children.last_mut();
        }

        Ok(WaitCmd(children))
    }

    fn run_cmd(&mut self, current_dir: &mut String) -> CmdResult {
        self.spawn(current_dir, false)?.wait_result_nolog()
    }

    fn run_fun(&mut self, current_dir: &mut String) -> FunResult {
        WaitFun(self.spawn(current_dir, true)?.0).wait_result_nolog()
    }
}

enum ProcHandle {
    ProcChild(Option<Child>), // for normal commands
    ProcBuf(Option<Vec<u8>>), // for builtin/custom commands
}

pub struct WaitCmd(Vec<(ProcHandle, String)>);
impl WaitCmd {
    pub fn wait_result(&mut self) -> CmdResult {
        let full_cmd = self
            .0
            .iter()
            .map(|cmd| cmd.1.to_owned())
            .collect::<Vec<_>>()
            .join(" | ");
        let ret = self.wait_result_nolog();
        if let Err(ref err) = ret {
            error!("Running {} failed, Error: {}", full_cmd, err);
        }
        ret
    }

    fn wait_result_nolog(&mut self) -> CmdResult {
        // wait last process result
        let (handle, cmd) = self.0.pop().unwrap();
        match handle {
            ProcHandle::ProcChild(child_opt) => {
                if let Some(mut child) = child_opt {
                    let status_result = child.wait();
                    Self::log_stderr(&mut child);
                    match status_result {
                        Err(e) => {
                            let _ = Self::wait_children(&mut self.0);
                            return Err(e);
                        }
                        Ok(status) => {
                            if !status.success() {
                                let _ = Self::wait_children(&mut self.0);
                                return Err(Self::status_to_io_error(
                                    status,
                                    &format!("{} exited with error", cmd),
                                ));
                            }
                        }
                    }
                }
            }
            ProcHandle::ProcBuf(mut ss) => {
                if let Some(s) = ss.take() {
                    let result = std::io::stdout().write_all(&s);
                    if let Err(e) = result {
                        let _ = Self::wait_children(&mut self.0);
                        return Err(e);
                    }
                }
            }
        }
        Self::wait_children(&mut self.0)
    }

    fn wait_children(children: &mut Vec<(ProcHandle, String)>) -> CmdResult {
        while !children.is_empty() {
            let (child_handle, cmd) = children.pop().unwrap();
            if let ProcHandle::ProcChild(Some(mut child)) = child_handle {
                let status = child.wait()?;
                Self::log_stderr(&mut child);
                if !status.success() && std::env::var("CMD_LIB_PIPEFAIL") != Ok("0".into()) {
                    return Err(Self::status_to_io_error(
                        status,
                        &format!("{} exited with error", cmd),
                    ));
                }
            }
        }
        Ok(())
    }

    fn log_stderr(child: &mut Child) {
        if let Some(stderr) = child.stderr.take() {
            WaitFun::log_stderr_output(stderr);
        }
    }

    fn status_to_io_error(status: ExitStatus, command: &str) -> Error {
        if let Some(code) = status.code() {
            Error::new(
                ErrorKind::Other,
                format!("{}; status code: {}", command, code),
            )
        } else {
            Error::new(
                ErrorKind::Other,
                format!("{}; terminated by {}", command, status),
            )
        }
    }
}

pub struct WaitFun(Vec<(ProcHandle, String)>);
impl WaitFun {
    fn wait_output(handle: &mut (ProcHandle, String)) -> Result<Vec<u8>> {
        match handle {
            (ProcHandle::ProcChild(child_opt), cmd) => {
                if let Some(child) = child_opt.take() {
                    let output = child.wait_with_output()?;
                    Self::log_stderr_output(&output.stderr[..]);
                    if !output.status.success() {
                        return Err(WaitCmd::status_to_io_error(
                            output.status,
                            &format!("{} exited with error", cmd),
                        ));
                    } else {
                        return Ok(output.stdout);
                    }
                }
            }
            (ProcHandle::ProcBuf(ss), _) => {
                if let Some(s) = ss.take() {
                    return Ok(s);
                }
            }
        }
        Ok(vec![])
    }

    pub fn wait_raw_result(&mut self) -> Result<Vec<u8>> {
        let ret = self.wait_raw_result_nolog();
        if let Err(ref err) = ret {
            error!("Running {} failed, Error: {}", self.get_full_cmd(), err);
        }
        ret
    }

    pub fn wait_raw_result_nolog(&mut self) -> Result<Vec<u8>> {
        let mut handle = self.0.pop().unwrap();
        let wait_last = Self::wait_output(&mut handle);
        match wait_last {
            Err(e) => {
                let _ = WaitCmd::wait_children(&mut self.0);
                Err(e)
            }
            Ok(output) => {
                WaitCmd::wait_children(&mut self.0)?;
                Ok(output)
            }
        }
    }

    fn get_full_cmd(&self) -> String {
        self.0
            .iter()
            .map(|cmd| cmd.1.to_owned())
            .collect::<Vec<_>>()
            .join(" | ")
    }

    pub fn wait_result(&mut self) -> FunResult {
        let ret = self.wait_result_nolog();
        if let Err(ref err) = ret {
            error!("Running {} failed, Error: {}", self.get_full_cmd(), err);
        }
        ret
    }

    pub fn wait_result_nolog(&mut self) -> FunResult {
        // wait last process result
        let mut handle = self.0.pop().unwrap();
        let wait_last = Self::wait_output(&mut handle);
        match wait_last {
            Err(e) => {
                let _ = WaitCmd::wait_children(&mut self.0);
                Err(e)
            }
            Ok(output) => {
                let mut ret = String::from_utf8_lossy(&output).to_string();
                if ret.ends_with('\n') {
                    ret.pop();
                }
                WaitCmd::wait_children(&mut self.0)?;
                Ok(ret)
            }
        }
    }

    fn log_stderr_output(output: impl Read) {
        BufReader::new(output)
            .lines()
            .filter_map(|line| line.ok())
            .for_each(|line| info!("{}", line));
    }
}

#[doc(hidden)]
pub enum Redirect {
    FileToStdin(String),
    StdoutToStderr,
    StderrToStdout,
    StdoutToFile(String, bool),
    StderrToFile(String, bool),
}
impl fmt::Debug for Redirect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Redirect::FileToStdin(path) => f.write_str(&format!("< {}", path)),
            Redirect::StdoutToStderr => f.write_str(">&2"),
            Redirect::StderrToStdout => f.write_str("2>&1"),
            Redirect::StdoutToFile(path, append) => {
                if *append {
                    f.write_str(&format!("1>> {}", path))
                } else {
                    f.write_str(&format!("1> {}", path))
                }
            }
            Redirect::StderrToFile(path, append) => {
                if *append {
                    f.write_str(&format!("2>> {}", path))
                } else {
                    f.write_str(&format!("2> {}", path))
                }
            }
        }
    }
}

#[doc(hidden)]
pub struct Cmd {
    // for parsing
    in_cmd_map: bool,
    args: Vec<String>,
    envs: HashMap<String, String>,
    redirects: Vec<Redirect>,

    // for running
    std_cmd: Option<Command>,
    stdin_redirect: Option<File>,
    stdout_redirect: Option<File>,
    stderr_redirect: Option<File>,
}

impl Default for Cmd {
    fn default() -> Self {
        Cmd {
            in_cmd_map: true,
            args: vec![],
            envs: HashMap::new(),
            redirects: vec![],
            stdin_redirect: None,
            stdout_redirect: None,
            stderr_redirect: None,
            std_cmd: None,
        }
    }
}

impl Cmd {
    pub fn add_arg(mut self, arg: String) -> Self {
        if self.args.is_empty() {
            let v: Vec<&str> = arg.split('=').collect();
            if v.len() == 2 && v[0].chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                self.envs.insert(v[0].to_owned(), v[1].to_owned());
                return self;
            }
            self.in_cmd_map = CMD_MAP.lock().unwrap().contains_key(arg.as_str());
        }
        self.args.push(arg);
        self
    }

    pub fn add_args(mut self, args: Vec<String>) -> Self {
        for arg in args {
            self = self.add_arg(arg);
        }
        self
    }

    pub fn add_redirect(mut self, redirect: Redirect) -> Self {
        self.redirects.push(redirect);
        self
    }

    fn arg0(&self) -> &str {
        if self.args.is_empty() {
            ""
        } else {
            &self.args[0]
        }
    }

    fn debug_str(&self) -> String {
        let mut ret = format!("{:?}", self.args);
        let mut extra = String::new();
        if !self.envs.is_empty() {
            extra += &format!("{:?}", self.envs);
        }
        if !self.redirects.is_empty() {
            if !extra.is_empty() {
                extra += ", ";
            }
            extra += &format!("{:?}", self.redirects);
        }
        if !extra.is_empty() {
            ret += &format!("({})", extra);
        }
        ret
    }

    fn gen_command(mut self) -> Self {
        if self.in_cmd_map {
            self
        } else {
            let cmds: Vec<String> = self.args.to_vec();
            let mut cmd = Command::new(&cmds[0]);
            cmd.args(&cmds[1..]);
            cmd.stdout(Stdio::piped()); // set stdout piped() by default, update later if needed
            cmd.stderr(Stdio::piped());
            for (k, v) in self.envs.iter() {
                cmd.env(k, v);
            }
            self.std_cmd = Some(cmd);
            self
        }
    }

    fn spawn(
        &mut self,
        current_dir: &mut String,
        with_output: bool,
        is_first: bool,
        is_last: bool,
        prev_child: &mut Option<&mut (ProcHandle, String)>,
    ) -> Result<(ProcHandle, String)> {
        if self.arg0() == "cd" {
            self.run_cd_cmd(current_dir)?;
            Ok((ProcHandle::ProcBuf(None), self.debug_str()))
        } else if self.in_cmd_map {
            let mut env = CmdEnv::new(&self.args, &self.envs, &current_dir);

            // setup stdin
            if is_first {
                if let Some(mut input) = self.stdin_redirect.take() {
                    input.read_to_end(&mut env.inbuf)?;
                }
            } else {
                env.inbuf = WaitFun::wait_output(&mut prev_child.take().unwrap())?;
            }

            let internal_cmd = CMD_MAP.lock().unwrap()[self.arg0()];
            internal_cmd(&mut env)?;

            // setup stderr
            if let Some(mut output_err) = self.stderr_redirect.take() {
                output_err.write_all(&env.errbuf)?;
            } else {
                WaitFun::log_stderr_output(&env.errbuf[..]);
            }

            // setup stdout
            if let Some(mut output) = self.stdout_redirect.take() {
                output.write_all(&env.outbuf)?;
                Ok((ProcHandle::ProcBuf(None), self.debug_str()))
            } else {
                Ok((ProcHandle::ProcBuf(Some(env.outbuf)), self.debug_str()))
            }
        } else {
            let mut cmd = self.std_cmd.take().unwrap();

            // setup current_dir
            if !current_dir.is_empty() {
                cmd.current_dir(current_dir.clone());
            }

            // update stdin
            if !is_first {
                let mut stdin_setup_done = false;
                if let Some((ProcHandle::ProcChild(Some(child)), _)) = prev_child {
                    if let Some(output) = child.stdout.take() {
                        cmd.stdin(output);
                        stdin_setup_done = true;
                    }
                }
                if !stdin_setup_done {
                    cmd.stdin(Stdio::piped());
                }
            }

            // update stdout
            if is_last && !with_output && self.stdout_redirect.is_none() {
                cmd.stdout(Stdio::inherit());
            }

            // spawning process
            let mut child = cmd.spawn()?;
            if !is_first {
                if let (ProcHandle::ProcBuf(ss), _) = prev_child.take().unwrap() {
                    if let Some(s) = ss.take() {
                        if let Some(mut input) = child.stdin.take() {
                            input.write_all(&s)?;
                        }
                    }
                }
            }
            Ok((ProcHandle::ProcChild(Some(child)), self.debug_str()))
        }
    }

    fn run_cd_cmd(&self, current_dir: &mut String) -> CmdResult {
        if self.args.len() == 1 {
            return Err(Error::new(ErrorKind::Other, "cd: missing directory"));
        } else if self.args.len() > 2 {
            let err_msg = format!("cd: too many arguments: {}", self.debug_str());
            return Err(Error::new(ErrorKind::Other, err_msg));
        }

        let dir = &self.args[1];
        if !std::path::Path::new(&dir).is_dir() {
            let err_msg = format!("cd: {}: No such file or directory", dir);
            error!("{}", err_msg);
            return Err(Error::new(ErrorKind::Other, err_msg));
        }

        if let Err(e) = Path::new(dir).access(AccessMode::EXECUTE) {
            error!("cd {}: {}", dir, e);
            return Err(e);
        }

        *current_dir = dir.clone();
        Ok(())
    }

    fn open_file(path: &str, read_only: bool, append: bool) -> Result<File> {
        if read_only {
            OpenOptions::new().read(true).open(path)
        } else {
            OpenOptions::new()
                .create(true)
                .truncate(!append)
                .write(true)
                .append(append)
                .open(path)
        }
    }

    fn setup_redirects(&mut self) -> CmdResult {
        let mut stdout_file = "/dev/stdout";
        let mut stderr_file = "/dev/stderr";
        for redirect in self.redirects.iter() {
            match redirect {
                Redirect::FileToStdin(path) => {
                    let file = Self::open_file(path, true, false)?;
                    if let Some(cmd) = self.std_cmd.as_mut() {
                        cmd.stdin(file.try_clone()?);
                    }
                    self.stdin_redirect = Some(file);
                }
                Redirect::StdoutToStderr => {
                    let file = if let Some(ref f) = self.stderr_redirect {
                        f.try_clone()?
                    } else {
                        Self::open_file(stderr_file, false, true)?
                    };
                    if let Some(cmd) = self.std_cmd.as_mut() {
                        cmd.stdout(file.try_clone()?);
                    }
                    self.stdout_redirect = Some(file);
                }
                Redirect::StderrToStdout => {
                    let file = if let Some(ref f) = self.stdout_redirect {
                        f.try_clone()?
                    } else {
                        Self::open_file(stdout_file, false, true)?
                    };
                    if let Some(cmd) = self.std_cmd.as_mut() {
                        cmd.stderr(file.try_clone()?);
                    }
                    self.stderr_redirect = Some(file);
                }
                Redirect::StdoutToFile(path, append) => {
                    let file = Self::open_file(path, false, *append)?;
                    if let Some(cmd) = self.std_cmd.as_mut() {
                        cmd.stdout(file.try_clone()?);
                    }
                    stdout_file = path;
                    self.stdout_redirect = Some(file);
                }
                Redirect::StderrToFile(path, append) => {
                    let file = Self::open_file(path, false, *append)?;
                    if let Some(cmd) = self.std_cmd.as_mut() {
                        cmd.stderr(file.try_clone()?);
                    }
                    stderr_file = path;
                    self.stderr_redirect = Some(file);
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_run_piped_cmds() {
        let mut current_dir = String::new();
        assert!(Cmds::default()
            .pipe(Cmd::default().add_args(vec!["echo".into(), "rust".into()]))
            .pipe(Cmd::default().add_args(vec!["wc".into()]))
            .run_cmd(&mut current_dir)
            .is_ok());
    }

    #[test]
    fn test_run_piped_funs() {
        let mut current_dir = String::new();
        assert_eq!(
            Cmds::default()
                .pipe(Cmd::default().add_args(vec!["echo".into(), "rust".into()]))
                .run_fun(&mut current_dir)
                .unwrap(),
            "rust"
        );

        assert_eq!(
            Cmds::default()
                .pipe(Cmd::default().add_args(vec!["echo".into(), "rust".into()]))
                .pipe(Cmd::default().add_args(vec!["wc".into(), "-c".into()]))
                .run_fun(&mut current_dir)
                .unwrap()
                .trim(),
            "5"
        );
    }

    #[test]
    fn test_stdout_redirect() {
        let mut current_dir = String::new();
        let tmp_file = "/tmp/file_echo_rust";
        let mut write_cmd = Cmd::default().add_args(vec!["echo".into(), "rust".into()]);
        write_cmd = write_cmd.add_redirect(Redirect::StdoutToFile(tmp_file.to_string(), false));
        assert!(Cmds::default()
            .pipe(write_cmd)
            .run_cmd(&mut current_dir)
            .is_ok());

        let read_cmd = Cmd::default().add_args(vec!["cat".into(), tmp_file.into()]);
        assert_eq!(
            Cmds::default()
                .pipe(read_cmd)
                .run_fun(&mut current_dir)
                .unwrap(),
            "rust"
        );

        let cleanup_cmd = Cmd::default().add_args(vec!["rm".into(), tmp_file.into()]);
        assert!(Cmds::default()
            .pipe(cleanup_cmd)
            .run_cmd(&mut current_dir)
            .is_ok());
    }
}
