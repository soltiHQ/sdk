use std::{fs, path::Path, sync::OnceLock, time::Instant};

static AGENT_ID: OnceLock<String> = OnceLock::new();
static START_TIME: OnceLock<Instant> = OnceLock::new();

/// Initialize agent start time.
pub fn init_uptime() {
    START_TIME.get_or_init(Instant::now);
}

/// Get agent uptime in seconds.
pub fn uptime_seconds() -> u64 {
    let start = START_TIME.get_or_init(Instant::now);
    start.elapsed().as_secs()
}

/// Get platform (OS family).
#[inline]
pub fn platform() -> &'static str {
    std::env::consts::OS
}

/// Get architecture.
#[inline]
pub fn arch() -> &'static str {
    std::env::consts::ARCH
}

/// Get or generate persistent agent ID.
pub fn agent_id() -> &'static str {
    AGENT_ID.get_or_init(|| {
        if is_kubernetes()
            && let Ok(hostname) = hostname::get()
            && let Some(name) = hostname.to_str()
        {
            return name.to_string();
        }
        if let Some(container_id) = get_container_id() {
            return container_id;
        }
        load_or_generate_id().unwrap_or_else(|_| uuid::Uuid::new_v4().to_string())
    })
}

/// Get OS distribution info (Linux only, best effort).
///
/// Returns OS name from `/etc/os-release` or generic platform name.
pub fn os_info() -> String {
    #[cfg(target_os = "linux")]
    {
        if let Ok(content) = fs::read_to_string("/etc/os-release") {
            for line in content.lines() {
                if let Some(name) = line.strip_prefix("PRETTY_NAME=") {
                    return name.trim_matches('"').to_string();
                }
            }
        }
    }

    platform().to_string()
}

fn load_or_generate_id() -> Result<String, std::io::Error> {
    let paths = [
        "/var/lib/solti/agent-id",
        &format!(
            "{}/.solti/agent-id",
            std::env::var("HOME").unwrap_or_default()
        ),
    ];
    for path in &paths {
        if let Ok(id) = fs::read_to_string(path) {
            let id = id.trim();
            if !id.is_empty() {
                return Ok(id.to_string());
            }
        }
    }

    let new_id = uuid::Uuid::new_v4().to_string();
    for path in &paths {
        if let Some(parent) = Path::new(path).parent() {
            let _ = fs::create_dir_all(parent);
        }
        if fs::metadata(path).is_ok() {
            break;
        }
    }
    Ok(new_id)
}

fn is_kubernetes() -> bool {
    std::env::var("KUBERNETES_SERVICE_HOST").is_ok()
        || Path::new("/var/run/secrets/kubernetes.io/serviceaccount").exists()
}

fn get_container_id() -> Option<String> {
    if Path::new("/.dockerenv").exists()
        && let Some(id) = parse_container_id_from_cgroup()
    {
        return Some(id);
    }
    parse_container_id_from_cgroup()
}

fn parse_container_id_from_cgroup() -> Option<String> {
    let cgroup = fs::read_to_string("/proc/self/cgroup").ok()?;

    for line in cgroup.lines() {
        if let Some(docker_part) = line.split('/').find(|s| s.starts_with("docker-")) {
            let id = docker_part
                .trim_start_matches("docker-")
                .trim_end_matches(".scope");
            if !id.is_empty() {
                return Some(id.to_string());
            }
        }
        if let Some(id) = line
            .split("/docker/")
            .nth(1)
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
        {
            return Some(id.to_owned());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_agent_id_stable() {
        let id1 = agent_id();
        let id2 = agent_id();
        assert_eq!(id1, id2);
        assert!(!id1.is_empty());
    }

    #[test]
    fn test_platform() {
        assert!(!platform().is_empty());
    }
}
