//! Share URIs.
//!
//! One URI per share, typed into the connect dialog or stored in the mount
//! table. The grammar is deliberately small: `<scheme>://[user[:password]@]host[:port]/resource`
//! with a query of known option names. A password may appear in the URI because
//! people paste them from chat, but it is split out into a [`Secret`] on the way
//! in and never stored in the table, so it cannot leak into a log line, a
//! `ps` listing or a saved file.
//!
//! The `resource` means something different per protocol — a share plus an
//! optional subdirectory for SMB, an export path for NFS, a directory for the
//! rest — but it is always stored normalised: leading `/`, no trailing `/`,
//! percent-decoded, and free of `.`, `..` and empty components.

use std::fmt;

use percent_encoding::percent_decode_str;
use serde::{Deserialize, Serialize};
use ufm_core::error::{Error, Result};

use crate::credentials::Secret;

/// Longest URI accepted, so a paste of a whole file cannot allocate without
/// bound.
const MAX_INPUT: usize = 2048;

/// A network protocol a share can use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    /// Windows/Samba shares, mounted with the kernel `cifs` filesystem.
    Smb,
    /// Kernel NFS.
    Nfs,
    /// SFTP, mounted through `sshfs`.
    Sftp,
    /// WebDAV, with or without TLS.
    WebDav,
    /// FTP, with or without implicit TLS. There is no kernel FTP, so this is
    /// always a FUSE mount.
    Ftp,
}

impl Protocol {
    /// Order used by the protocol chooser in the connect dialog.
    pub const ALL: &[Self] = &[Self::Smb, Self::Nfs, Self::Sftp, Self::WebDav, Self::Ftp];

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Smb => "SMB/CIFS",
            Self::Nfs => "NFS",
            Self::Sftp => "SFTP",
            Self::WebDav => "WebDAV",
            Self::Ftp => "FTP",
        }
    }

    /// The scheme written into a location. TLS is spelled with a trailing `s`
    /// for the two protocols where it is a choice, so a displayed URI re-parses
    /// to the same share.
    #[must_use]
    pub const fn scheme(self) -> &'static str {
        match self {
            Self::Smb => "smb",
            Self::Nfs => "nfs",
            Self::Sftp => "sftp",
            Self::WebDav => "webdav",
            Self::Ftp => "ftp",
        }
    }

    /// The scheme for a URI, including the `s` for `davs` and `ftps`.
    #[must_use]
    pub const fn uri_scheme(self, tls: bool) -> &'static str {
        match (self, tls) {
            (Self::WebDav, true) => "davs",
            (Self::Ftp, true) => "ftps",
            _ => self.scheme(),
        }
    }

    /// The scheme a HTTP-family helper dials.
    #[must_use]
    pub const fn wire_scheme(self, tls: bool) -> &'static str {
        match (self, tls) {
            (Self::WebDav | Self::Ftp, true) => "https",
            (Self::WebDav, false) => "http",
            (Self::Ftp, false) => "ftp",
            _ => self.scheme(),
        }
    }

    #[must_use]
    pub const fn icon_name(self) -> &'static str {
        match self {
            Self::Smb => "network-workgroup-symbolic",
            // NFS and FTP both read as "a server on the network"; there is no
            // separate symbolic icon worth shipping for either.
            Self::Nfs | Self::Ftp => "network-server-symbolic",
            Self::Sftp => "folder-remote-symbolic",
            Self::WebDav => "folder-network-symbolic",
        }
    }

    /// What to install when the helper for this protocol is missing. Shown
    /// verbatim in the connect dialog, so it names `apk` packages.
    #[must_use]
    pub const fn packages(self) -> &'static str {
        match self {
            Self::Smb => "cifs-utils",
            Self::Nfs => "nfs-utils",
            Self::Sftp => "sshfs-fuse",
            Self::WebDav => "davfs2 or rclone",
            Self::Ftp => "rclone or lftp",
        }
    }

    /// Whether `tls` means anything for this protocol. SFTP is always
    /// encrypted; SMB and NFS are not offered encrypted here.
    #[must_use]
    pub const fn supports_tls(self) -> bool {
        matches!(self, Self::WebDav | Self::Ftp)
    }

    #[must_use]
    pub const fn default_port(self, tls: bool) -> u16 {
        match self {
            Self::Smb => 445,
            Self::Nfs => 2049,
            Self::Sftp => 22,
            Self::WebDav => {
                if tls {
                    443
                } else {
                    80
                }
            }
            Self::Ftp => {
                if tls {
                    990
                } else {
                    21
                }
            }
        }
    }

    /// Whether a password is meaningful. SFTP authenticates with a key or the
    /// agent, and NFS with the client's identity.
    #[must_use]
    pub const fn uses_password(self) -> bool {
        matches!(self, Self::Smb | Self::WebDav | Self::Ftp)
    }

    /// Option names accepted in the query, besides `port`, which belongs to the
    /// authority.
    #[must_use]
    pub const fn options(self) -> &'static [&'static str] {
        match self {
            Self::Smb => &["vers", "domain", "ro", "uid", "gid", "label"],
            Self::Nfs => &["vers", "ro", "proto", "label"],
            Self::Sftp => &["idmap", "ro", "label"],
            Self::WebDav => &["ro", "slot", "label"],
            Self::Ftp => &["ro", "label"],
        }
    }
}

/// A parsed share, without its password. This is what the mount table stores.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Share {
    pub protocol: Protocol,
    /// Lowercased and percent-decoded. An IPv6 literal is stored *without* its
    /// brackets; [`Share::source`] and [`Share::backend_url`] add them back, and
    /// [`Share::slug`] turns the colons into dashes.
    pub host: String,
    /// `None` means the protocol's default, which `davs://` and `ftps://` shift.
    pub port: Option<u16>,
    pub user: Option<String>,
    /// True for `davs://` and `ftps://`.
    #[serde(default)]
    pub tls: bool,
    /// Share and subdirectory for SMB, export path for NFS, directory for the
    /// rest. Always absolute, never trailing.
    pub resource: String,
    /// Protocol options given in the query, in the order written.
    #[serde(default)]
    pub options: Vec<(String, String)>,
}

/// A URI split into the part that is safe to store and the password.
#[derive(Debug)]
pub struct ParsedUri {
    pub share: Share,
    pub password: Option<Secret>,
}

impl Share {
    /// Parse a share URI. See the module docs for the grammar.
    pub fn parse(input: &str) -> Result<ParsedUri> {
        let input = input.trim();
        if input.is_empty() {
            return Err(Error::InvalidLocation("no address given".into()));
        }
        if input.len() > MAX_INPUT {
            return Err(Error::InvalidLocation("address is too long".into()));
        }
        let (scheme, rest) = input.split_once("://").ok_or_else(|| {
            Error::InvalidLocation(format!(
                "{input}: expected an address like smb://host/share"
            ))
        })?;
        let (protocol, tls) = scheme_of(scheme)?;
        // Windows-style separators are pasted constantly for SMB, and a
        // backslash is legal in neither a host nor a URL path. The replaced
        // string has to be bound: the slices below borrow out of it.
        let slashes = rest.replace('\\', "/");
        let (authority, resource, query) = split_address(&slashes, input)?;
        let (userinfo, host, port) = split_authority(authority, input)?;
        let (user, password) = split_userinfo(userinfo);
        let resource = normalise_resource(resource, protocol, input)?;
        let options = parse_options(query, protocol, input)?;
        let share = Share {
            protocol,
            host,
            port,
            user,
            tls,
            resource,
            options,
        };
        Ok(ParsedUri {
            share,
            password: password.map(Secret::from),
        })
    }

    /// The port actually used.
    #[must_use]
    pub fn port_used(&self) -> u16 {
        self.port
            .unwrap_or_else(|| self.protocol.default_port(self.tls))
    }

    /// The address without a password, suitable for display and for the table.
    #[must_use]
    pub fn display(&self) -> String {
        let mut out = String::with_capacity(32 + self.host.len() + self.resource.len());
        out.push_str(self.protocol.uri_scheme(self.tls));
        out.push_str("://");
        if let Some(user) = &self.user {
            out.push_str(user);
            out.push('@');
        }
        out.push_str(&self.host);
        if self.port.is_some() {
            out.push(':');
            out.push_str(&self.port_used().to_string());
        }
        out.push_str(&self.resource);
        out
    }

    /// The last component of the resource, which is the share name for SMB and
    /// the deepest directory otherwise. Never empty.
    #[must_use]
    pub fn leaf(&self) -> &str {
        self.resource
            .rsplit('/')
            .next()
            .filter(|s| !s.is_empty())
            .unwrap_or(&self.host)
    }

    /// The option value for `key`, if the URI carried one.
    #[must_use]
    pub fn option(&self, key: &str) -> Option<&str> {
        self.options
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// Whether the mount should be read-only, from `?ro` or `?ro=1`.
    #[must_use]
    pub fn read_only(&self) -> bool {
        match self.option("ro") {
            None => false,
            Some(v) => v.is_empty() || v == "1" || v == "true",
        }
    }

    /// A short, filesystem-safe, stable name for the mount point and the table.
    ///
    /// The hash of the canonical address keeps two shares with the same leaf
    /// name on different hosts, or two directories on one host, apart, and keeps
    /// a name with no ASCII in it usable as a path component.
    #[must_use]
    pub fn slug(&self) -> String {
        let host = sanitize(&self.host.replace([':', '.'], "-"));
        let leaf = sanitize(self.leaf());
        let mut out = String::with_capacity(host.len() + leaf.len() + 8);
        out.push_str(host.trim_matches('-'));
        if !leaf.is_empty() && leaf != host {
            out.push('-');
            out.push_str(leaf.trim_matches('-'));
        }
        if !out.is_empty() {
            out.push('-');
        }
        out.push_str(&self.tag());
        out
    }

    /// Six hex digits of the share's identity, used in the slug and the record
    /// id so a user can tell apart two shares that display the same label.
    #[must_use]
    pub fn tag(&self) -> String {
        const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
        const PRIME: u64 = 0x100_0000_01b3;
        let mut hash = OFFSET;
        for bytes in [self.display().as_bytes(), self.protocol.scheme().as_bytes()] {
            for byte in bytes {
                hash ^= u64::from(*byte);
                hash = hash.wrapping_mul(PRIME);
            }
        }
        format!("{:06x}", hash & 0xff_ffff)
    }

    /// The source string the kernel or helper is given.
    #[must_use]
    pub fn source(&self) -> String {
        match self.protocol {
            // The kernel cifs helper wants UNC syntax, with the subdirectory
            // appended: `//srv/Users/alice/Docs`. An IPv6 address keeps its
            // brackets, or the colons read as a port.
            Protocol::Smb => format!("//{}{}", wrap6(&self.host), self.resource),
            Protocol::Nfs => format!("{}:{}", wrap6(&self.host), self.resource),
            Protocol::Sftp => match &self.user {
                Some(user) => format!("{user}@{}:{}", wrap6(&self.host), self.resource),
                None => format!("{}:{}", wrap6(&self.host), self.resource),
            },
            Protocol::WebDav | Protocol::Ftp => self.backend_url(),
        }
    }

    /// The URL a FUSE helper dials: scheme chosen by `tls`, port always
    /// explicit so a non-default port cannot be silently dropped.
    #[must_use]
    pub fn backend_url(&self) -> String {
        format!(
            "{}://{}:{}{}",
            self.protocol.wire_scheme(self.tls),
            wrap6(&self.host),
            self.port_used(),
            self.resource
        )
    }

    /// The label to show in the Network group: the user's, else the leaf, else
    /// the host.
    #[must_use]
    pub fn label(&self) -> String {
        if let Some(label) = self.option("label").filter(|l| !l.is_empty()) {
            return label.to_string();
        }
        if self.leaf() == self.host {
            self.host.clone()
        } else {
            self.leaf().to_string()
        }
    }
}

impl fmt::Display for Share {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.display())
    }
}

fn scheme_of(scheme: &str) -> Result<(Protocol, bool)> {
    let (protocol, tls) = match scheme.to_ascii_lowercase().as_str() {
        "smb" | "cifs" => (Protocol::Smb, false),
        "nfs" | "nfsmount" => (Protocol::Nfs, false),
        "sftp" | "ssh" => (Protocol::Sftp, false),
        "webdav" | "dav" => (Protocol::WebDav, false),
        "davs" | "webdavs" => (Protocol::WebDav, true),
        "ftp" => (Protocol::Ftp, false),
        "ftps" => (Protocol::Ftp, true),
        other => {
            return Err(Error::InvalidLocation(format!(
                "{other}:// is not a network share; use one of {}",
                Protocol::ALL
                    .iter()
                    .map(|p| p.scheme())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
    };
    Ok((protocol, tls))
}

/// `(authority, resource, query)`. The resource keeps its leading slash.
fn split_address<'a>(rest: &'a str, input: &str) -> Result<(&'a str, &'a str, &'a str)> {
    let (rest, query) = match rest.split_once('?') {
        Some((a, q)) => (a, q),
        None => (rest, ""),
    };
    let (authority, resource) = match rest.split_once('/') {
        Some((a, r)) => (a, r),
        None => (rest, ""),
    };
    if authority.is_empty() {
        return Err(Error::InvalidLocation(format!("{input}: no host given")));
    }
    Ok((authority, resource, query))
}

fn split_authority<'a>(
    authority: &'a str,
    input: &str,
) -> Result<(Option<&'a str>, String, Option<u16>)> {
    let (userinfo, hostport) = match authority.rsplit_once('@') {
        Some((u, h)) => (Some(u), h),
        None => (None, authority),
    };
    // An IPv6 literal is bracketed, so the colons inside it cannot be read as
    // a port separator.
    let (host, port) = if let Some(rest) = hostport.strip_prefix('[') {
        let (host, port) = rest.split_once(']').ok_or_else(|| {
            Error::InvalidLocation(format!("{input}: a bracketed address needs a closing ]"))
        })?;
        (host, parse_port(port.strip_prefix(':'), input)?)
    } else {
        match hostport.split_once(':') {
            Some((h, p)) => (h, parse_port(Some(p), input)?),
            None => (hostport, None),
        }
    };
    if host.is_empty() {
        return Err(Error::InvalidLocation(format!("{input}: no host given")));
    }
    let host = percent_decode_str(host).decode_utf8_lossy().into_owned();
    if !host
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b':' | b'%'))
    {
        return Err(Error::InvalidLocation(format!(
            "{input}: {host} is not a valid host name"
        )));
    }
    Ok((userinfo, host.to_ascii_lowercase(), port))
}

fn parse_port(raw: Option<&str>, input: &str) -> Result<Option<u16>> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    if raw.is_empty() {
        return Ok(None);
    }
    if !raw.bytes().all(|b| b.is_ascii_digit()) {
        return Err(Error::InvalidLocation(format!(
            "{input}: {raw} is not a port number"
        )));
    }
    let value: u32 = raw.parse().map_err(|_| {
        Error::InvalidLocation(format!(
            "{input}: {raw} is not a port number between 1 and 65535"
        ))
    })?;
    if value == 0 || value > u32::from(u16::MAX) {
        return Err(Error::InvalidLocation(format!(
            "{input}: {raw} is not a port number between 1 and 65535"
        )));
    }
    Ok(Some(value as u16))
}

/// Everything before the host. A user name may legitimately contain a `:` — a
/// Windows UPN is `user@domain`, which is why the host is split on the *last*
/// `@` — so only the first `:` separates the password.
fn split_userinfo(userinfo: Option<&str>) -> (Option<String>, Option<String>) {
    let Some(userinfo) = userinfo else {
        return (None, None);
    };
    if userinfo.is_empty() {
        return (None, None);
    }
    let (user, password) = match userinfo.split_once(':') {
        Some((u, p)) => (u, Some(p)),
        None => (userinfo, None),
    };
    let user = percent_decode_str(user).decode_utf8_lossy().into_owned();
    let password = password.map(|p| percent_decode_str(p).decode_utf8_lossy().into_owned());
    (
        (!user.is_empty()).then_some(user),
        password.filter(|p| !p.is_empty()),
    )
}

/// Percent-decode the path and reject anything that could escape the share.
fn normalise_resource(raw: &str, protocol: Protocol, input: &str) -> Result<String> {
    let decoded = percent_decode_str(raw).decode_utf8_lossy();
    let mut parts: Vec<&str> = Vec::new();
    for part in decoded.split('/') {
        match part {
            // Skipped: a trailing slash, a doubled slash, and `.` all mean the
            // same directory.
            "" | "." => {}
            ".." => {
                return Err(Error::UnsafeComponent(format!(
                    "{input}: .. cannot be used inside a share address"
                )));
            }
            other if other.contains('\0') => {
                return Err(Error::UnsafeComponent(format!(
                    "{input}: a name cannot contain a NUL"
                )));
            }
            other => parts.push(other),
        }
    }
    if parts.is_empty() {
        // SMB needs a share name; the others can be mounted at their root.
        if protocol == Protocol::Smb {
            return Err(Error::InvalidLocation(format!(
                "{input}: a share name is required, like smb://server/documents"
            )));
        }
        return Ok(String::new());
    }
    let mut out = String::with_capacity(decoded.len() + parts.len());
    for part in &parts {
        out.push('/');
        out.push_str(part);
    }
    Ok(out)
}

fn parse_options(query: &str, protocol: Protocol, input: &str) -> Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (key, value) = match pair.split_once('=') {
            Some((k, v)) => (k, percent_decode_str(v).decode_utf8_lossy().into_owned()),
            None => (pair, String::new()),
        };
        let key = key.to_ascii_lowercase();
        if key == "port" {
            continue; // belongs to the authority
        }
        if !protocol.options().contains(&key.as_str()) {
            return Err(Error::InvalidLocation(format!(
                "{input}: {key} is not an option for {}; allowed: {}",
                protocol.label(),
                protocol.options().join(", ")
            )));
        }
        if value.contains([',', '\0']) {
            return Err(Error::InvalidLocation(format!(
                "{input}: {key} cannot contain a comma"
            )));
        }
        out.push((key, value));
    }
    Ok(out)
}

/// Keep a name usable as a path component: ASCII alphanumerics, `-`, `_` and
/// `.`, with everything else collapsed to a single `-`. Accented or non-Latin
/// names can collapse to nothing, which is why every slug also carries a hash.
fn sanitize(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut last_dash = true;
    for b in name.bytes() {
        if b.is_ascii_alphanumeric() || b == b'_' || b == b'.' {
            out.push(char::from(b));
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    out.truncate(40);
    out
}

fn wrap6(host: &str) -> String {
    if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(input: &str) -> ParsedUri {
        Share::parse(input).unwrap_or_else(|e| panic!("{input}: {e}"))
    }

    fn share(input: &str) -> Share {
        parsed(input).share
    }

    #[test]
    fn smb_share_and_subdirectory() {
        let s = share("smb://SERVER/Users/alice/Notes");
        assert_eq!(s.protocol, Protocol::Smb);
        assert_eq!(s.host, "server");
        assert_eq!(s.resource, "/Users/alice/Notes");
        assert_eq!(s.source(), "//server/Users/alice/Notes");
        assert_eq!(s.port_used(), 445);
        assert_eq!(s.display(), "smb://server/Users/alice/Notes");
        assert_eq!(s.label(), "Notes");
    }

    #[test]
    fn windows_separators_and_percent_escapes() {
        let s = share("smb://srv/Public/My%20Docs");
        assert_eq!(s.resource, "/Public/My Docs");
        let s = share(r"smb://srv\Public\Reports");
        assert_eq!(s.resource, "/Public/Reports");
    }

    #[test]
    fn a_password_is_split_out_and_never_displayed() {
        let p = parsed("webdav://alice:s3cr3t@dav.example.org/files");
        assert_eq!(p.share.user.as_deref(), Some("alice"));
        assert_eq!(p.password.as_ref().map(Secret::expose), Some("s3cr3t"));
        assert!(
            !p.share.display().contains("s3cr3t"),
            "{}",
            p.share.display()
        );
        assert!(!format!("{}", p.share).contains("s3cr3t"));
        assert!(!format!("{:?}", p.share).contains("s3cr3t"));
    }

    #[test]
    fn an_empty_password_is_no_password() {
        let p = parsed("webdav://alice:@host/files");
        assert_eq!(p.share.user.as_deref(), Some("alice"));
        assert!(p.password.is_none());
    }

    #[test]
    fn a_user_may_contain_an_at_sign() {
        // A Windows UPN, where the host separator is the last '@'.
        let p = parsed("smb://corp%5Calice@domain@filesrv/share");
        assert_eq!(p.share.user.as_deref(), Some("corp\\alice@domain"));
        assert_eq!(p.share.host, "filesrv");
        assert!(p.password.is_none());
    }

    #[test]
    fn tls_schemes_change_the_default_port_and_url() {
        let plain = share("webdav://dav.example.org/files");
        assert_eq!(plain.port_used(), 80);
        assert_eq!(plain.backend_url(), "http://dav.example.org:80/files");
        let secure = share("davs://dav.example.org/files");
        assert!(secure.tls);
        assert_eq!(secure.port_used(), 443);
        assert_eq!(secure.backend_url(), "https://dav.example.org:443/files");
        assert_eq!(secure.display(), "davs://dav.example.org/files");
        let ftps = share("ftps://ftp.example.org/pub");
        assert_eq!(ftps.port_used(), 990);
        assert_eq!(ftps.protocol, Protocol::Ftp);
        assert_eq!(ftps.backend_url(), "https://ftp.example.org:990/pub");
    }

    #[test]
    fn an_explicit_port_survives_and_is_shown() {
        let s = share("sftp://alice@host.example:2222/data");
        assert_eq!(s.port, Some(2222));
        assert_eq!(s.port_used(), 2222);
        assert_eq!(s.display(), "sftp://alice@host.example:2222/data");
        assert_eq!(s.source(), "alice@host.example:/data");
    }

    #[test]
    fn ipv6_hosts_are_bracketed() {
        let s = share("nfs://[fe80::1%25251]/srv/data");
        assert_eq!(s.host, "fe80::1%251");
        assert_eq!(s.source(), "[fe80::1%251]:/srv/data");
        let s = share("smb://[::1]/share");
        assert_eq!(s.host, "::1");
        assert_eq!(s.source(), "//[::1]/share");
        assert!(
            s.slug().starts_with("1-share-"),
            "unexpected slug {}",
            s.slug()
        );
    }

    #[test]
    fn nfs_root_export_is_allowed() {
        let s = share("nfs://host/");
        assert_eq!(s.resource, "");
        assert_eq!(s.source(), "host:");
        assert_eq!(s.label(), "host");
    }

    #[test]
    fn options_are_checked_against_the_protocol() {
        let s = share("smb://srv/share?vers=2.0&domain=WORK&ro");
        assert_eq!(s.option("vers"), Some("2.0"));
        assert_eq!(s.option("domain"), Some("WORK"));
        assert!(s.read_only());
        assert!(!share("smb://srv/share?vers=2.0").read_only());
        let err = Share::parse("smb://srv/share?proto=tcp").unwrap_err();
        assert!(err.to_string().contains("not an option"), "{err}");
        // The same option is fine on NFS.
        assert!(Share::parse("nfs://host/export?proto=tcp").is_ok());
    }

    #[test]
    fn a_comma_in_an_option_would_break_the_mount_line() {
        let err = Share::parse("smb://srv/share?vers=3,0").unwrap_err();
        assert!(err.to_string().contains("comma"), "{err}");
    }

    #[test]
    fn traversal_and_empty_are_rejected() {
        for bad in ["smb://srv/../etc", "smb://srv/a/..", "webdav://host/..%2f"] {
            let err = Share::parse(bad).unwrap_err();
            assert!(
                matches!(err, Error::UnsafeComponent(_) | Error::InvalidLocation(_)),
                "{bad} accepted"
            );
        }
        assert!(matches!(
            Share::parse("smb://srv/"),
            Err(Error::InvalidLocation(_))
        ));
        assert!(matches!(
            Share::parse("http://srv/x"),
            Err(Error::InvalidLocation(_))
        ));
        assert!(matches!(
            Share::parse("srv/share"),
            Err(Error::InvalidLocation(_))
        ));
        assert!(matches!(Share::parse(""), Err(Error::InvalidLocation(_))));
        assert!(matches!(
            Share::parse("smb://"),
            Err(Error::InvalidLocation(_))
        ));
    }

    #[test]
    fn a_bad_port_is_reported_not_panicked() {
        for bad in [
            "smb://srv:0/share",
            "smb://srv:70000/share",
            "smb://srv:http/share",
            "smb://[::1/share",
        ] {
            assert!(Share::parse(bad).is_err(), "{bad} should not parse");
        }
    }

    #[test]
    fn slugs_are_short_safe_and_unique() {
        let a = share("smb://filesrv.local/Users/alice/Notes").slug();
        let b = share("smb://filesrv.local/Users/bob/Notes").slug();
        let c = share("smb://backup.local/Users/alice/Notes").slug();
        for slug in [&a, &b, &c] {
            assert!(slug.len() <= 60, "{slug}");
            assert!(
                slug.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.')),
                "{slug}"
            );
            assert!(
                slug.starts_with("filesrv-local-") || slug.starts_with("backup-local-"),
                "{slug}"
            );
        }
        assert_ne!(a, b, "same leaf on one host must not collide");
        assert_ne!(a, c, "same path on another host must not collide");
    }

    #[test]
    fn a_name_with_no_ascii_still_yields_a_usable_slug() {
        let s = share("smb://srv/\u{9879}\u{76ee}\u{5f55}");
        assert_eq!(s.slug(), format!("srv-{}", s.tag()));
        assert!(
            s.slug()
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-'),
            "{}",
            s.slug()
        );
    }

    #[test]
    fn a_label_can_be_given_or_taken_from_the_leaf() {
        assert_eq!(
            share("webdav://host/files?label=Projects").label(),
            "Projects"
        );
        assert_eq!(share("nfs://host/a/b/c").label(), "c");
    }

    #[test]
    fn every_protocol_round_trips_through_display() {
        for protocol in Protocol::ALL {
            for tls in [false, true] {
                let uri = format!("{}://user@host.example/data/set", protocol.uri_scheme(tls));
                let s = share(&uri);
                assert_eq!(s.protocol, *protocol);
                assert_eq!(s.tls, protocol.supports_tls() && tls);
                assert_eq!(s.display(), uri, "display must re-parse");
                let again = share(&s.display());
                assert_eq!(again, s);
                assert!(!s.slug().is_empty());
            }
        }
    }

    #[test]
    fn a_long_input_is_refused_before_allocating() {
        let uri = format!("smb://srv/{}", "a".repeat(MAX_INPUT));
        assert!(matches!(Share::parse(&uri), Err(Error::InvalidLocation(_))));
    }
}
