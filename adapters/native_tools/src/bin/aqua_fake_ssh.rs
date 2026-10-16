//! A stand-in for `ssh`, `scp`, `ssh-keyscan` and `ssh-keygen`, used only by the tests.
//!
//! The tests copy this program to files named `ssh.exe`, `scp.exe`, `ssh-keyscan.exe` and
//! `ssh-keygen.exe`; it behaves like the one its file name says. It prints the arguments it was
//! given (so tests can check exactly what Aqua passed) and acts on the remote "command" it is
//! sent, to simulate failures. It never touches the network.

use std::io::{Read, Write};

fn main() {
    let exe = std::env::current_exe().unwrap();
    let name = exe.file_stem().unwrap().to_string_lossy().to_lowercase();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let dir = exe.parent().unwrap().to_path_buf();
    // The real ssh-keyscan and ssh-keygen print only their result, so only ssh and scp echo their input.
    if name == "ssh" || name == "scp" {
        println!("ARGV:{}", serde_json::to_string(&args).unwrap());
        let mut env_names: Vec<String> = std::env::vars().map(|(k, _)| k).collect();
        env_names.sort();
        println!("ENV:{}", env_names.join(","));
    }

    match name.as_str() {
        "docker" => {
            use std::io::Write as _;
            // Every call is appended to docker.log so tests can see exactly what Aqua passed.
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("docker.log")) {
                let _ = writeln!(f, "{}", serde_json::to_string(&args).unwrap());
            }
            if dir.join("docker_down").exists() {
                eprintln!("Cannot connect to the Docker daemon at npipe:////./pipe/docker_engine. Is the docker daemon running?");
                std::process::exit(1);
            }
            let sub = args.first().cloned().unwrap_or_default();
            let last = args.last().cloned().unwrap_or_default();
            match sub.as_str() {
                "ps" => {
                    println!(r#"{{"ID":"aaaaaaaaaaaa1111","Image":"nginx:1.25","Names":"web","State":"running","Status":"Up 2 hours","Ports":"0.0.0.0:8080->80/tcp","Labels":""}}"#);
                    println!(r#"{{"ID":"bbbbbbbbbbbb2222","Image":"alpine:3.19","Names":"aqua-job","State":"exited","Status":"Exited (0) 1 hour ago","Ports":"","Labels":"aqua.managed=true,other=x"}}"#);
                }
                "images" => println!(r#"{{"Repository":"alpine","Tag":"3.19","ID":"sha256:c0ffee","Size":"7MB","CreatedSince":"2 weeks ago"}}"#),
                "network" => println!(r#"{{"Name":"bridge","Driver":"bridge","Scope":"local"}}"#),
                "volume" => println!(r#"{{"Name":"data","Driver":"local"}}"#),
                "container" => {
                    if last == "missing" {
                        eprintln!("Error response from daemon: No such container: missing");
                        std::process::exit(1);
                    }
                    if args.iter().any(|a| a == "-f") {
                        println!("{}", if last.starts_with("aqua-") { r#"{"aqua.managed":"true"}"# } else { "{}" });
                    } else {
                        println!(r#"[{{"Id":"sha256:abcdef0123456789","Name":"/{last}","Config":{{"Image":"alpine:3.19","Env":["DB_PASSWORD=hunter2-secret","MODE=prod"],"Labels":{{"aqua.managed":"true"}}}},"State":{{"Status":"running","StartedAt":"2026-09-30T10:00:00Z","ExitCode":0}},"HostConfig":{{"Privileged":false,"NetworkMode":"bridge","Memory":536870912}},"Mounts":[{{"Type":"bind","Source":"/host/data","Destination":"/data","RW":false}}],"NetworkSettings":{{"Ports":{{}}}}}}]"#);
                    }
                }
                "logs" => {
                    println!("starting up");
                    println!("error: password=abc123456789 leaked in a log line");
                }
                "stats" => println!(r#"{{"Name":"aqua-job","CPUPerc":"0.10%","MemUsage":"10MiB / 512MiB"}}"#),
                "start" | "stop" | "restart" | "rm" | "rmi" => println!("{last}"),
                "pull" => println!("docker.io/library/{last}"),
                "run" => println!("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"),
                "exec" => println!("exec output from {}", args.get(1).cloned().unwrap_or_default()),
                "compose" => println!("compose ok: {}", args.join(" ")),
                _ => {
                    eprintln!("unknown docker command");
                    std::process::exit(1)
                }
            }
        }
        "kubectl" => {
            use std::io::Write as _;
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("kubectl.log")) {
                let _ = writeln!(f, "{}", serde_json::to_string(&args).unwrap());
            }
            if dir.join("cluster_down").exists() {
                eprintln!("Unable to connect to the server: dial tcp 10.0.0.1:6443: i/o timeout");
                std::process::exit(1);
            }
            let rest: Vec<&String> = args.iter().filter(|a| !a.starts_with("--")).collect();
            let sub = rest.first().map(|s| s.as_str()).unwrap_or("");
            match sub {
                "get" => println!("NAME   READY   STATUS
web-1  1/1     Running"),
                "describe" => println!("Name: web-1
Containers:
  app:
    Image: nginx
    Environment:
      DB_PASSWORD:  hunter2-secret
    Mounts:  <none>
Events:  <none>"),
                "logs" => println!("started
error: password=abc123456789 leaked"),
                "top" => println!("NAME CPU MEMORY
web-1 1m 10Mi"),
                "apply" => {
                    if args.iter().any(|a| a == "--dry-run=server") {
                        println!("deployment.apps/web configured (server dry run)");
                    } else {
                        println!("deployment.apps/web configured");
                    }
                }
                "scale" => println!("deployment.apps/web scaled"),
                "rollout" => println!("rolled: {}", rest.get(1).map(|s| s.as_str()).unwrap_or("")),
                "delete" => println!("{} deleted", rest.get(2).map(|s| s.as_str()).unwrap_or("")),
                _ => {
                    eprintln!("error: unknown command");
                    std::process::exit(1)
                }
            }
        }
        "terraform" => {
            use std::io::Write as _;
            let mut env_names: Vec<String> = std::env::vars().map(|(k, _)| k).collect();
            env_names.sort();
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("terraform.log")) {
                let cwd = std::env::current_dir().map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
                let _ = writeln!(f, "{}", serde_json::to_string(&serde_json::json!({"args": args, "cwd": cwd, "env": env_names})).unwrap());
            }
            if std::env::current_dir().map(|d| d.join("locked").exists()).unwrap_or(false) {
                eprintln!("Error: Error acquiring the state lock");
                std::process::exit(1);
            }
            let sub = args.first().cloned().unwrap_or_default();
            match sub.as_str() {
                "init" => println!("Terraform has been successfully initialized!"),
                "validate" => {
                    if std::env::current_dir().map(|d| d.join("invalid").exists()).unwrap_or(false) {
                        println!(r#"{{"valid":false,"diagnostics":[{{"severity":"error","summary":"Unsupported argument"}}]}}"#);
                    } else {
                        println!(r#"{{"valid":true,"diagnostics":[]}}"#);
                    }
                }
                "plan" => {
                    let out = args.iter().position(|a| a == "-out").and_then(|i| args.get(i + 1)).cloned().unwrap_or_default();
                    let body = std::fs::read_to_string(dir.join("plan_body.txt")).unwrap_or_else(|_| "PLAN-V1".into());
                    std::fs::write(&out, body).unwrap();
                    println!("Plan: 1 to add, 1 to change, 0 to destroy.");
                }
                "show" => println!("{}", std::fs::read_to_string(dir.join("show.json")).unwrap_or_else(|_| r#"{"resource_changes":[{"address":"aws_s3_bucket.a","change":{"actions":["create"]}},{"address":"aws_instance.b","change":{"actions":["update"]}}]}"#.into())),
                "apply" => println!("Apply complete! key={}", std::env::var("AWS_ACCESS_KEY_ID").unwrap_or_default()),
                "output" => println!(r#"{{"url":{{"value":"https://x.example","sensitive":false}},"db_password":{{"value":"hunter2-secret","sensitive":true}}}}"#),
                "state" => println!("aws_s3_bucket.a\naws_instance.b"),
                _ => {
                    eprintln!("Error: unknown command");
                    std::process::exit(1)
                }
            }
        }
        "aws" | "az" | "gcloud" => {
            use std::io::Write as _;
            let mut env_names: Vec<String> = std::env::vars().map(|(k, _)| k).collect();
            env_names.sort();
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("cloud.log")) {
                let _ = writeln!(f, "{}", serde_json::to_string(&serde_json::json!({"tool": name, "args": args, "env": env_names})).unwrap());
            }
            if dir.join("cloud_denied").exists() {
                eprintln!("An error occurred (UnauthorizedOperation) when calling the DescribeInstances operation: You are not authorized");
                std::process::exit(255);
            }
            println!(r#"{{"Reservations":[{{"Instance":"i-1","Environment":{{"Variables":{{"DB":"postgres://u:pw@h/db"}}}},"note":"key={}"}}]}}"#, std::env::var("AWS_ACCESS_KEY_ID").unwrap_or_default());
        }
        "ssh-keyscan" => {
            let host = args.last().cloned().unwrap_or_default();
            if host == "keyscan-fail" {
                eprintln!("getaddrinfo keyscan-fail: Name or service not known");
                std::process::exit(1);
            }
            let port = args.iter().position(|a| a == "-p").and_then(|i| args.get(i + 1)).cloned().unwrap_or_else(|| "22".into());
            let key = std::fs::read_to_string(dir.join("key.txt")).unwrap_or_else(|_| "FAKEKEYA".into());
            let who = if port == "22" { host } else { format!("[{host}]:{port}") };
            println!("{who} ssh-ed25519 AAAAC3NzaC1lZDI1NTE5{}", key.trim());
        }
        "ssh-keygen" => {
            let mut line = String::new();
            std::io::stdin().read_to_string(&mut line).ok();
            let key = line.split_whitespace().nth(2).unwrap_or("none");
            println!("256 SHA256:fp-{key} host (ED25519)");
        }
        "scp" => {
            if args.iter().any(|a| a.contains("fail-scp")) {
                eprintln!("scp: fail-scp: No such file or directory");
                std::process::exit(1);
            }
            // A download creates the local file, as the real program would.
            if let Some(remote_at) = args.iter().position(|a| a.contains('@') && a.contains(':')) {
                if remote_at + 1 < args.len() && remote_at > 0 && args[remote_at - 1] == "--" {
                    std::fs::write(&args[remote_at + 1], "downloaded content").ok();
                }
            }
        }
        _ => {
            let cmd = args.last().cloned().unwrap_or_default();
            match cmd.as_str() {
                "hostkey-unknown" => {
                    eprintln!("Host key verification failed.");
                    std::process::exit(255)
                }
                "hostkey-changed" => {
                    eprintln!("@@@ WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED! @@@");
                    std::process::exit(255)
                }
                "denied" => {
                    eprintln!("deploy@203.0.113.5: Permission denied (publickey).");
                    std::process::exit(255)
                }
                "refused" => {
                    eprintln!("ssh: connect to host 203.0.113.5 port 22: Connection refused");
                    std::process::exit(255)
                }
                "exit7" => std::process::exit(7),
                "sleep" => std::thread::sleep(std::time::Duration::from_secs(60)),
                "big" => {
                    let line = "x".repeat(1000);
                    let mut o = std::io::stdout();
                    for _ in 0..300 {
                        writeln!(o, "{line}").ok();
                    }
                }
                "echo aqua-ok" => println!("aqua-ok"),
                other => println!("ok: {other}"),
            }
        }
    }
}
