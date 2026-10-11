//! Lossless native path encoding and purely lexical operations. No operation in
//! this module consults the working directory, environment, or filesystem.

use std::ffi::{OsStr, OsString};
use std::path::{Component, Path, PathBuf};

use crate::Value;

/// Encode native units: bytes on Unix, little-endian UTF-16 code units on Windows.
pub(crate) fn encode(path: &OsStr) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        path.as_bytes().to_vec()
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        path.encode_wide().flat_map(u16::to_le_bytes).collect()
    }
}

/// Decode native units without a lossy Unicode conversion, rejecting NULs.
pub(crate) fn decode(bytes: &[u8]) -> Result<PathBuf, i64> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        if bytes.contains(&0) {
            return Err(2);
        }
        Ok(OsString::from_vec(bytes.to_vec()).into())
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStringExt;
        if !bytes.len().is_multiple_of(2) {
            return Err(2);
        }
        let units: Vec<_> =
            bytes.chunks_exact(2).map(|p| u16::from_le_bytes([p[0], p[1]])).collect();
        if units.contains(&0) {
            return Err(2);
        }
        Ok(OsString::from_wide(&units).into())
    }
}

fn normalize(path: &Path) -> PathBuf {
    // Verbatim Windows paths disable normal Win32 normalization. In particular,
    // dot components can be literal names; preserve their complete spelling.
    if matches!(path.components().next(), Some(Component::Prefix(p)) if p.kind().is_verbatim()) {
        return path.to_owned();
    }
    let mut result = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => match result.components().next_back() {
                Some(Component::Normal(_)) => {
                    result.pop();
                }
                _ if !path.has_root() => result.push(".."),
                _ => {}
            },
            other => result.push(other.as_os_str()),
        }
    }
    if result.as_os_str().is_empty() && !path.as_os_str().is_empty() {
        result.push(".");
    }
    result
}

fn relative_to(base: &Path, target: &Path) -> Result<PathBuf, i64> {
    let base = normalize(base);
    let target = normalize(target);
    let a: Vec<_> = base.components().filter(|p| *p != Component::CurDir).collect();
    let b: Vec<_> = target.components().filter(|p| *p != Component::CurDir).collect();
    let prefix = |parts: &[Component<'_>]| {
        parts
            .iter()
            .take_while(|p| matches!(p, Component::Prefix(_) | Component::RootDir))
            .map(|p| p.as_os_str().to_owned())
            .collect::<Vec<_>>()
    };
    if prefix(&a) != prefix(&b) {
        return Err(3);
    }
    let common = a.iter().zip(&b).take_while(|(a, b)| a == b).count();
    if a[common..].contains(&Component::ParentDir) {
        return Err(3);
    }
    let mut result = PathBuf::new();
    for _ in &a[common..] {
        result.push("..");
    }
    for part in &b[common..] {
        result.push(part.as_os_str());
    }
    if result.as_os_str().is_empty() {
        result.push(".");
    }
    Ok(result)
}

fn single_name(path: &Path) -> bool {
    let mut parts = path.components();
    matches!(parts.next(), Some(Component::Normal(_)))
        && parts.next().is_none()
        && path.file_name() == Some(path.as_os_str())
}

/// Status 1 is an absent component/unrepresentable text, 2 invalid units/name,
/// 3 an incompatible base. The std wrapper maps these into its typed surface.
fn operation(op: i64, a: &[u8], b: &[u8]) -> Result<Vec<u8>, i64> {
    if op == 0 {
        let text = std::str::from_utf8(a).map_err(|_| 1)?;
        if text.contains('\0') {
            return Err(2);
        }
        return Ok(encode(OsStr::new(text)));
    }
    if op == 17 {
        return Ok(if cfg!(windows) { b"windows".to_vec() } else { b"unix".to_vec() });
    }
    let path = decode(a)?;
    let output = match op {
        1 => return path.to_str().map(|s| s.as_bytes().to_vec()).ok_or(1),
        // Debug's escaped representation is diagnostic-only and unambiguous even
        // when a name contains controls, quotes, or invalid Unicode units.
        2 => return Ok(format!("{:?}", path.as_os_str()).into_bytes()),
        3 => return Ok(a.to_vec()),
        4 => return if path.is_absolute() { Ok(Vec::new()) } else { Err(1) },
        5 => path.parent().ok_or(1)?.to_owned(),
        6 => PathBuf::from(path.file_name().ok_or(1)?),
        7 => PathBuf::from(path.file_stem().ok_or(1)?),
        8 => PathBuf::from(path.extension().ok_or(1)?),
        9 => path.join(decode(b)?),
        10 => normalize(&path),
        11 => {
            let child = decode(b)?;
            if !path.is_absolute() || (child.has_prefix() && !child.is_absolute()) {
                return Err(3);
            }
            normalize(&path.join(child))
        }
        12 => relative_to(&path, &decode(b)?)?,
        13 => {
            let child = decode(b)?;
            if !single_name(&child) || path.file_name().is_none() {
                return Err(2);
            }
            path.with_file_name(child)
        }
        14 => {
            let child = decode(b)?;
            if (!child.as_os_str().is_empty() && !single_name(&child)) || path.file_name().is_none()
            {
                return Err(2);
            }
            path.with_extension(child)
        }
        15 => {
            let root: PathBuf = path
                .components()
                .take_while(|p| matches!(p, Component::Prefix(_) | Component::RootDir))
                .collect();
            if root.as_os_str().is_empty() {
                return Err(1);
            }
            root
        }
        _ => return Err(2),
    };
    Ok(encode(output.as_os_str()))
}

trait HasPrefix {
    fn has_prefix(&self) -> bool;
}
impl HasPrefix for Path {
    fn has_prefix(&self) -> bool {
        matches!(self.components().next(), Some(Component::Prefix(_)))
    }
}

/// Native lexical path operation over owned byte buffers. Returns (status, bytes).
#[unsafe(no_mangle)]
pub extern "C" fn fai_path_operation(op: Value, a: Value, b: Value) -> Value {
    // SAFETY: the intrinsic's two buffer operands have static type Bytes.
    let result = operation(crate::unbox_int(op), unsafe { crate::bytes_bytes(a) }, unsafe {
        crate::bytes_bytes(b)
    });
    crate::fai_drop(op);
    crate::fai_drop(a);
    crate::fai_drop(b);
    let (status, bytes) = match result {
        Ok(bytes) => (0, bytes),
        Err(status) => (status, Vec::new()),
    };
    // SAFETY: the tuple receives two owned values.
    unsafe {
        crate::fai_make_data(0, 2, [crate::imm_int(status), crate::make_bytes(&bytes)].as_ptr())
    }
}

/// Split a native path into its lexical components, returning owned byte buffers.
#[unsafe(no_mangle)]
pub extern "C" fn fai_path_components(value: Value) -> Value {
    // SAFETY: the intrinsic's operand has static type Bytes.
    let path = decode(unsafe { crate::bytes_bytes(value) });
    let parts: Vec<_> = path
        .map(|p| p.components().map(|c| crate::make_bytes(&encode(c.as_os_str()))).collect())
        .unwrap_or_default();
    crate::fai_drop(value);
    let array = crate::alloc_array(parts.len(), parts.len());
    for (index, part) in parts.into_iter().enumerate() {
        // SAFETY: each allocated slot receives one owned Bytes value.
        unsafe { crate::write_i64(array, crate::ARRAY_ELEMS_OFFSET + index * 8, part) };
    }
    crate::from_obj(array)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn op(code: i64, a: &str, b: &str) -> Result<PathBuf, i64> {
        operation(code, &encode(OsStr::new(a)), &encode(OsStr::new(b))).and_then(|b| decode(&b))
    }
    #[test]
    fn empty_is_distinct_from_dot() {
        assert_eq!(op(10, "", ""), Ok(PathBuf::new()));
    }
    #[test]
    fn normalization_cancels_only_normal_components() {
        assert_eq!(op(10, "a/../../b/./c", ""), Ok(PathBuf::from("../b/c")));
    }
    #[test]
    fn roots_have_no_parent() {
        assert_eq!(op(5, if cfg!(windows) { "C:\\" } else { "/" }, ""), Err(1));
    }
    #[test]
    fn dotfiles_have_no_extension() {
        assert_eq!(op(8, ".gitignore", ""), Err(1));
    }
    #[test]
    fn relative_paths_share_an_explicit_base() {
        assert_eq!(op(12, "a/b", "a/c/d"), Ok(PathBuf::from("../c/d")));
    }
    #[test]
    fn unresolved_parent_base_is_rejected() {
        assert_eq!(op(12, "../a", "b"), Err(3));
    }
    #[test]
    fn filenames_cannot_contain_separators() {
        assert_eq!(op(13, "a/b", "c/d"), Err(2));
    }
    #[test]
    fn nul_is_rejected() {
        assert_eq!(operation(0, b"a\0b", b""), Err(2));
    }
    #[cfg(unix)]
    #[test]
    fn non_utf8_names_round_trip() {
        let bytes = b"/tmp/\xff";
        assert_eq!(encode(decode(bytes).unwrap().as_os_str()), bytes);
        assert_eq!(operation(1, bytes, b""), Err(1));
        assert!(String::from_utf8(operation(2, bytes, b"").unwrap()).unwrap().contains("\\xFF"));
    }
    #[cfg(windows)]
    #[test]
    fn unpaired_utf16_round_trips() {
        let bytes = [0x00, 0xd8];
        assert_eq!(encode(decode(&bytes).unwrap().as_os_str()), bytes);
        assert_eq!(operation(1, &bytes, b""), Err(1));
    }
    #[cfg(windows)]
    #[test]
    fn odd_native_encoding_is_rejected() {
        assert_eq!(decode(&[65]), Err(2));
    }
    #[cfg(windows)]
    #[test]
    fn drive_relative_resolution_is_rejected() {
        assert_eq!(op(11, "C:\\base", "C:file"), Err(3));
    }
    #[cfg(windows)]
    #[test]
    fn unc_normalization_stays_within_share() {
        assert_eq!(
            op(10, "\\\\server\\share\\..\\x", ""),
            Ok(PathBuf::from("\\\\server\\share\\x"))
        );
    }
    #[cfg(windows)]
    #[test]
    fn verbatim_paths_preserve_dot_spelling() {
        let path = "\\\\?\\C:\\a\\..\\b";
        assert_eq!(op(10, path, ""), Ok(PathBuf::from(path)));
    }

    mod proptests {
        use super::*;
        use proptest::prelude::*;
        proptest! {
            #[test]
            fn native_units_round_trip(bytes in proptest::collection::vec(any::<u8>(), 0..256)) {
                if let Ok(path) = decode(&bytes) { prop_assert_eq!(encode(path.as_os_str()), bytes); }
            }
            #[test]
            fn normalization_is_idempotent(parts in proptest::collection::vec("[a-z.]{1,8}", 0..20)) {
                let path = PathBuf::from(parts.join("/"));
                prop_assert_eq!(normalize(&normalize(&path)), normalize(&path));
            }
        }
    }
}
