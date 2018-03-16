use cli_flow;
use colored::Colorize;
use database::Database;
use difference::{Changeset, Difference};
use rpassword;
use ssh2::Session;
use std::env;
use std::io::Read;
use std::io::prelude::*;
use std::net::TcpStream;
use std::path::Path;
use std::str;

pub fn sync(db: &mut Database) {
    let mut syned_sth = false;

    for host in &mut db.hosts {
        // sync needed for host?
        if !host.sync_todo {
            continue;
        }

        cli_flow::infoln(&format!("Syncing host {}...", host.hostname));
        syned_sth = true;

        // ssh connect to host
        // defaults for connection
        let mut hostname = &*host.hostname;
        let mut port = "22";

        // hostname:port format?
        let host_splitted: Vec<&str> = host.hostname.split(':').collect();

        // found one ':' in hostname
        if host_splitted.len() == 2 {
            if host_splitted[1].parse::<i32>().is_ok() {
                hostname = &*host_splitted[0];
                port = &*host_splitted[1];
            }
        }

        // connect!
        let tcp = match TcpStream::connect(&format!("{}:{}", hostname, port)) {
            Ok(t) => t,
            Err(e) => {
                cli_flow::errorln(&e.to_string());
                continue;
            }
        };

        // promt for remote user
        let ssh_user_default = "root";
        let ssh_user = cli_flow::read_line(
            &format!("SSH User ({}):", ssh_user_default),
            &ssh_user_default,
        );

        // guess ssh key location
        let private_key_path = match env::home_dir() {
            Some(path) => path.join(".ssh").join("id_rsa"),
            None => Path::new("").to_path_buf(),
        };

        let mut private_key_file = String::from(private_key_path.to_str().unwrap());
        private_key_file = cli_flow::read_line(
            &format!("Private key ({}):", private_key_file),
            &private_key_file,
        );

        // prompt for passphrase
        cli_flow::prompt("Passphrase:");
        let private_key_pass = rpassword::prompt_password_stdout("").unwrap();

        // create ssh session
        let mut sess = match Session::new() {
            Some(s) => s,
            None => {
                cli_flow::errorln("Unable to creare SSH session.");
                continue;
            }
        };

        // ssh handshake
        match sess.handshake(&tcp) {
            Ok(h) => h,
            Err(e) => {
                cli_flow::errorln(&e.to_string());
                continue;
            }
        };

        // public key auth
        match sess.userauth_pubkey_file(
            &ssh_user,
            None,
            Path::new(&private_key_file),
            Some(&private_key_pass),
        ) {
            Ok(t) => t,
            Err(e) => {
                cli_flow::errorln(&e.to_string());
                continue;
            }
        }

        // drop passphrase
        drop(private_key_pass);

        // read current authorized_keys from host
        let mut channel = sess.channel_session().unwrap();
        channel.exec("echo $HOME").unwrap();
        let mut home = String::new();
        channel.read_to_string(&mut home).unwrap();

        let mut remote_authorized_keys_file = format!(
            "{}/.ssh/authorized_keys",
            home.trim_right().trim_left().to_owned()
        );
        channel.wait_close().is_ok();

        // prompt for remote authorized_keys file
        remote_authorized_keys_file = cli_flow::read_line(
            &format!("Remote authorized_keys ({}):", remote_authorized_keys_file),
            &remote_authorized_keys_file,
        );

        let authorized_keys_res = sess.scp_recv(Path::new(&remote_authorized_keys_file));
        let mut authorized_keys_remote = Vec::new();
        let mut authorized_keys_remote_str = "";

        if !authorized_keys_res.is_ok() {
            cli_flow::warningln(&format!(
                "Unable to read remote {} - {:?}",
                remote_authorized_keys_file,
                authorized_keys_res.err()
            ));
        } else {
            let (mut ch, _stat) = authorized_keys_res.unwrap();
            ch.read_to_end(&mut authorized_keys_remote).unwrap();

            authorized_keys_remote_str = match str::from_utf8(&authorized_keys_remote) {
                Ok(v) => v,
                Err(e) => {
                    cli_flow::warningln(&format!(
                        "{}: Invalid UTF-8 sequence: {}",
                        host.hostname, e
                    ));
                    continue;
                }
            };
        }

        // collect authorized_keys to sync ...
        let mut authorized_keys_sync_vec: Vec<String> = Vec::new();

        // ... 1. on user level
        for authorized_user_id in &host.authorized_users {
            for user in &db.users {
                if &user.user_id == authorized_user_id {
                    // build e.g.
                    // # mail@example.com
                    // ssh-rsa ...
                    authorized_keys_sync_vec.append(&mut vec![
                        format!(
                            "# {}\n{}",
                            authorized_user_id,
                            String::from(&*user.public_key)
                        ),
                    ]);
                }
            }
        }

        // ... 2. on group level
        for authorized_group_id in &host.authorized_user_groups {
            for group in &db.user_groups {
                if authorized_group_id == &group.group_id {
                    for user_id in &group.members {
                        for user in &db.users {
                            if user_id == &user.user_id {
                                authorized_keys_sync_vec.append(&mut vec![
                                    format!("# {}\n{}", user_id, String::from(&*user.public_key)),
                                ]);
                                break;
                            }
                        }
                    }
                    break;
                }
            }
        }

        authorized_keys_sync_vec.sort();
        authorized_keys_sync_vec.dedup();

        // show diff of authorized_keys of host <-> to sync
        let authorized_keys_sync_str = format!("{}\n", authorized_keys_sync_vec.join("\n\n"));
        let Changeset { diffs, .. } =
            Changeset::new(&authorized_keys_remote_str, &authorized_keys_sync_str, "\n");

        println!("");
        for i in 0..diffs.len() {
            match diffs[i] {
                Difference::Same(ref x) => {
                    println!("{}", x);
                }
                Difference::Add(ref x) => {
                    println!("{}", format!("+{}", x).green());
                }
                Difference::Rem(ref x) => {
                    println!("{}", format!("-{}", x).red());
                }
            }
        }

        // sync confirmation
        if cli_flow::prompt_yes_no("Verify changes. Do you want to sync? (y/n):") == "n" {
            cli_flow::warningln(&format!("Skipping sync of {} as you told so\n\n", hostname));
            continue;
        }

        // sync!
        let mut remote_file = sess.scp_send(
            Path::new(&remote_authorized_keys_file),
            0o600,
            authorized_keys_sync_str.len() as u64,
            None,
        ).unwrap();
        remote_file
            .write(authorized_keys_sync_str.as_bytes())
            .unwrap();

        cli_flow::okln(&format!(
            "Successfully synced to {}\n",
            remote_authorized_keys_file
        ));
        host.sync_todo = false;
    }

    if !syned_sth {
        cli_flow::okln("All hosts up to date. Nothing to sync, bye bye");
    }
}
