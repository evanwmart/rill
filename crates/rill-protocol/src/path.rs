use crate::MAX_PATH;
use crate::error::FrameError;

/// Validate a request path against the spec §7.1 rules.
///
/// There is deliberately no normalization: a path either satisfies every rule
/// as sent or the frame is rejected, so authorization always sees exactly the
/// bytes the client transmitted.
pub fn validate_path(path: &str) -> Result<(), FrameError> {
    let len = path.len();
    if len == 0 {
        return Err(FrameError::PathInvalid("empty"));
    }
    if len > MAX_PATH {
        return Err(FrameError::PathInvalid("longer than MAX_PATH"));
    }
    if !path.starts_with('/') {
        return Err(FrameError::PathInvalid("must start with '/'"));
    }
    if path.bytes().any(|b| b == 0) {
        return Err(FrameError::PathInvalid("contains NUL byte"));
    }
    if path == "/" {
        return Ok(());
    }
    if path.ends_with('/') {
        return Err(FrameError::PathInvalid("trailing '/'"));
    }
    for segment in path[1..].split('/') {
        match segment {
            "" => return Err(FrameError::PathInvalid("empty segment")),
            "." => return Err(FrameError::PathInvalid("'.' segment")),
            ".." => return Err(FrameError::PathInvalid("'..' segment")),
            _ => {}
        }
    }
    Ok(())
}
