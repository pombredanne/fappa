use std::env;
use std::fs;
use std::io::Read;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::AsRawFd;
use std::path;
use std::process;

use failure::ensure;
use failure::err_msg;
use failure::format_err;
use failure::Error;
use failure::ResultExt;
use std::ffi::CString;

pub mod child;

pub fn prepare(distro: &str) -> Result<child::Child, Error> {
    let root = format!("{}/root", distro);

    // TODO: do we need to do this unconditionally?
    if !path::Path::new(&root).is_dir() {
        fs::create_dir(&root)?;
        crate::unpack::unpack(&format!("{}/amd64-root.tar.gz", distro), &root)?;
    }

    let (mut from_recv, from_send) = os_pipe::pipe()?;
    let (into_recv, mut into_send) = os_pipe::pipe()?;

    {
        use std::os::unix::fs::PermissionsExt;
        let finit_host = format!("{}/bin/finit", root);
        reflink::reflink_or_copy("target/debug/finit", &finit_host)?;
        fs::set_permissions(&finit_host, fs::Permissions::from_mode(0o755))?;
        fs::write(
            format!("{}/etc/resolv.conf", root),
            b"nameserver 127.0.0.53",
        )?;
    }

    let first_fork = {
        use nix::unistd::*;
        match fork()? {
            ForkResult::Parent { child } => child,
            ForkResult::Child => match setup_namespace(&root, into_recv, from_send) {
                Ok(v) => void::unreachable(v),
                Err(e) => {
                    eprintln!("sandbox setup failed: {:?}", e);
                    process::exit(67);
                }
            },
        }
    };

    from_recv.read(&mut vec![0u8; 1])?;

    let real_euid = nix::unistd::geteuid();
    let real_egid = nix::unistd::getegid();

    // TODO: read 165536 from /etc/sub?id
    #[rustfmt::skip]
    ensure!(std::process::Command::new("newuidmap")
        .args(&[&format!("{}", first_fork),
            "0", &format!("{}", real_euid), "1",
            "1", "165536", "65535"
        ])
        .status()?.success(), "setting up newuidmap for worker");

    #[rustfmt::skip]
    ensure!(std::process::Command::new("newgidmap")
        .args(&[&format!("{}", first_fork),
            "0", &format!("{}", real_egid), "1",
            "1", "165536", "65535"
        ])
        .status()?.success(), "setting up newgidmap for worker");

    into_send.write_all(b"a")?;

    Ok(child::Child {
        recv: from_recv,
        send: into_send,
        pid: first_fork,
    })
}

fn reopen_stdin_as_null() -> Result<(), Error> {
    nix::unistd::dup3(
        fs::File::open("/dev/null")?.as_raw_fd(),
        0,
        nix::fcntl::OFlag::empty(),
    )?;

    Ok(())
}

fn setup_namespace(
    root: &str,
    mut recv: os_pipe::PipeReader,
    mut send: os_pipe::PipeWriter,
) -> Result<void::Void, Error> {
    use nix::unistd::*;

    reopen_stdin_as_null()?;

    {
        use nix::sched::*;
        unshare(
            CloneFlags::CLONE_NEWIPC
                | CloneFlags::CLONE_NEWNS
                | CloneFlags::CLONE_NEWPID
                | CloneFlags::CLONE_NEWUSER
                | CloneFlags::CLONE_NEWUTS,
        )
        .with_context(|_| err_msg("unshare"))?;
    }

    {
        let unset: Option<&str> = None;
        use nix::mount::*;

        mount(
            Some("none"),
            "/",
            unset,
            MsFlags::MS_REC | MsFlags::MS_PRIVATE,
            unset,
        )
        .with_context(|_| err_msg("mount --make-rprivate"))?;

        // mount our unpacked root on itself, inside the new namespace
        mount(
            Some(root),
            root,
            unset,
            MsFlags::MS_BIND | MsFlags::MS_NOSUID,
            unset,
        )
        .with_context(|_| err_msg("mount $root $root"))?;

        env::set_current_dir(root)?;

        // make /proc visible inside the chroot.
        // without this, `mount -t proc proc /proc` fails with EPERM.
        // No, I don't know where this is documented.
        make_mount_destination(".host-proc")?;
        mount(
            Some("/proc"),
            ".host-proc",
            unset,
            MsFlags::MS_BIND | MsFlags::MS_REC,
            unset,
        )
        .with_context(|_| err_msg("mount --bind /proc .host-proc"))?;

        mount(
            Some("/sys"),
            "sys",
            unset,
            MsFlags::MS_BIND | MsFlags::MS_REC,
            unset,
        )
        .with_context(|_| err_msg("mount --bind /sys sys"))?;

        drop(fs::File::create("dev/null")?);
        mount(
            Some("/dev/null"),
            "dev/null",
            unset,
            MsFlags::MS_BIND,
            unset,
        )
        .with_context(|_| err_msg("mount --bind /dev/null"))?;
    }

    {
        send.write_all(b"1")?;

        let mut buf = [0u8; 1];
        ensure!(
            1 == recv.read(&mut buf)?,
            "reading resume permission from host failed"
        );
    }

    setresuid(Uid::from_raw(0), Uid::from_raw(0), Uid::from_raw(0))
        .with_context(|_| err_msg("setuid"))?;
    setresgid(Gid::from_raw(0), Gid::from_raw(0), Gid::from_raw(0))
        .with_context(|_| err_msg("setgid"))?;

    setgroups(&[Gid::from_raw(0)]).with_context(|_| err_msg("setgroups(0)"))?;

    make_mount_destination("old")?;
    pivot_root(&Some("."), &Some("old")).with_context(|_| err_msg("pivot_root"))?;
    nix::mount::umount2("old", nix::mount::MntFlags::MNT_DETACH)
        .with_context(|_| err_msg("unmount old"))?;
    fs::remove_dir("old").with_context(|_| err_msg("rm old"))?;

    match fork()? {
        ForkResult::Parent { child: _ } => {
            use nix::sys::wait::*;
            // Mmm, not sure this is useful or even helpful.
            process::exit(match wait()? {
                WaitStatus::Exited(_, code) => code,
                _ => 66,
            });
        }

        ForkResult::Child => match setup_pid_1(recv, send) {
            Ok(v) => void::unreachable(v),
            Err(e) => {
                eprintln!("sandbox setup pid1 failed: {:?}", e);
                process::exit(67);
            }
        },
    }
}

fn setup_pid_1(recv: os_pipe::PipeReader, send: os_pipe::PipeWriter) -> Result<void::Void, Error> {
    use nix::unistd::*;

    {
        let us = getpid().as_raw();
        ensure!(1 == us, "we failed to actually end up as pid 1: {}", us);
    }

    {
        let sticky_for_all = fs::Permissions::from_mode(0o1777);
        fs::set_permissions("/tmp", sticky_for_all.clone())?;
        fs::set_permissions("/var/tmp", sticky_for_all)?;
        // TODO: dev/shm?
    }

    {
        let unset: Option<&str> = None;
        use nix::mount::*;

        mount(
            Some("/proc"),
            "/proc",
            Some("proc"),
            MsFlags::MS_NOSUID,
            unset,
        )
        .with_context(|_| err_msg("mount proc -t proc /proc"))?;

        umount2(".host-proc", MntFlags::MNT_DETACH)
            .with_context(|_| err_msg("unmount .host-proc"))?;

        fs::remove_dir(".host-proc")?;

        mount(
            Some("/"),
            "/",
            unset,
            MsFlags::MS_BIND | MsFlags::MS_NOSUID | MsFlags::MS_REMOUNT,
            unset,
        )
        .with_context(|_| err_msg("finalising /"))?;
    }

    let recv = dup(recv.as_raw_fd())?;
    let send = dup(send.as_raw_fd())?;

    let proc = CString::new("/bin/finit")?;
    let argv0 = proc.clone();
    let recv = CString::new(format!("{}", recv))?;
    let send = CString::new(format!("{}", send))?;

    void::unreachable(execv(&proc, &[argv0, recv, send]).with_context(|_| err_msg("exec finit"))?);
}

fn make_mount_destination(name: &'static str) -> Result<(), Error> {
    let _ = fs::remove_dir(name);
    fs::create_dir(name)
        .with_context(|_| format_err!("creating {} before mounting on it", name))?;
    fs::set_permissions(name, fs::Permissions::from_mode(0o644))?;
    Ok(())
}
