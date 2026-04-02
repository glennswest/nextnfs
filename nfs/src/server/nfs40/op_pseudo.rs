use nextnfs_proto::nfs4_proto::{
    Attrlist4, Entry4, Fattr4, FileAttr, FileAttrValue, FsLocations4, Fsid4, NfsFh4, NfsFtype4,
    NfsStat4, Nfstime4,
};

use crate::server::export_manager::ExportManagerHandle;

/// The export_id reserved for the pseudo-filesystem root.
pub const PSEUDO_ROOT_EXPORT_ID: u8 = 0xFF;

/// Build the pseudo-root filehandle — a synthetic handle representing
/// the NFSv4 root that lists exports as top-level directories.
pub fn pseudo_root_fh() -> NfsFh4 {
    let mut id = [0u8; 26];
    id[0] = 0x02; // version: pseudo-root
    id[1] = PSEUDO_ROOT_EXPORT_ID;
    // rest is zero
    id
}

/// Check if a filehandle is the pseudo-root.
pub fn is_pseudo_root(fh: &NfsFh4) -> bool {
    fh[0] == 0x02 && fh[1] == PSEUDO_ROOT_EXPORT_ID
}

/// Extract the export_id from a filehandle.
/// Returns PSEUDO_ROOT_EXPORT_ID for pseudo-root, or the value at fh[1] for real handles.
pub fn export_id_from_fh(fh: &NfsFh4) -> u8 {
    if is_pseudo_root(fh) {
        PSEUDO_ROOT_EXPORT_ID
    } else {
        fh[1]
    }
}

/// Stamp an export_id into an existing filehandle.
/// This sets fh[1] to the export_id so we can route requests.
pub fn stamp_export_id(fh: &mut NfsFh4, export_id: u8) {
    fh[1] = export_id;
}

/// Build a synthetic Filehandle-like attrs response for the pseudo-root.
pub fn pseudo_root_getattr(
    attr_request: &[FileAttr],
) -> (Attrlist4<FileAttr>, Attrlist4<FileAttrValue>) {
    let mut answer_attrs = Attrlist4::<FileAttr>::new(None);
    let mut attrs = Attrlist4::<FileAttrValue>::new(None);
    let now = current_time();

    for fileattr in attr_request {
        match fileattr {
            FileAttr::SupportedAttrs => {
                answer_attrs.push(FileAttr::SupportedAttrs);
                attrs.push(FileAttrValue::SupportedAttrs(pseudo_supported_attrs()));
            }
            FileAttr::Type => {
                answer_attrs.push(FileAttr::Type);
                attrs.push(FileAttrValue::Type(NfsFtype4::Nf4dir));
            }
            FileAttr::FhExpireType => {
                answer_attrs.push(FileAttr::FhExpireType);
                attrs.push(FileAttrValue::FhExpireType(
                    nextnfs_proto::nfs4_proto::FH4_PERSISTENT,
                ));
            }
            FileAttr::Change => {
                answer_attrs.push(FileAttr::Change);
                attrs.push(FileAttrValue::Change(1));
            }
            FileAttr::Size => {
                answer_attrs.push(FileAttr::Size);
                attrs.push(FileAttrValue::Size(4096));
            }
            FileAttr::LinkSupport => {
                answer_attrs.push(FileAttr::LinkSupport);
                attrs.push(FileAttrValue::LinkSupport(false));
            }
            FileAttr::SymlinkSupport => {
                answer_attrs.push(FileAttr::SymlinkSupport);
                attrs.push(FileAttrValue::SymlinkSupport(false));
            }
            FileAttr::NamedAttr => {
                answer_attrs.push(FileAttr::NamedAttr);
                attrs.push(FileAttrValue::NamedAttr(true));
            }
            FileAttr::Fsid => {
                answer_attrs.push(FileAttr::Fsid);
                attrs.push(FileAttrValue::Fsid(Fsid4 { major: 0, minor: 0 }));
            }
            FileAttr::UniqueHandles => {
                answer_attrs.push(FileAttr::UniqueHandles);
                attrs.push(FileAttrValue::UniqueHandles(true));
            }
            FileAttr::LeaseTime => {
                answer_attrs.push(FileAttr::LeaseTime);
                attrs.push(FileAttrValue::LeaseTime(90));
            }
            FileAttr::RdattrError => {
                answer_attrs.push(FileAttr::RdattrError);
                attrs.push(FileAttrValue::RdattrError(NfsStat4::Nfs4errInval));
            }
            FileAttr::Fileid => {
                answer_attrs.push(FileAttr::Fileid);
                attrs.push(FileAttrValue::Fileid(1));
            }
            FileAttr::Mode => {
                answer_attrs.push(FileAttr::Mode);
                attrs.push(FileAttrValue::Mode(0o755));
            }
            FileAttr::Numlinks => {
                answer_attrs.push(FileAttr::Numlinks);
                attrs.push(FileAttrValue::Numlinks(2));
            }
            FileAttr::Owner => {
                answer_attrs.push(FileAttr::Owner);
                attrs.push(FileAttrValue::Owner("0".to_string()));
            }
            FileAttr::OwnerGroup => {
                answer_attrs.push(FileAttr::OwnerGroup);
                attrs.push(FileAttrValue::OwnerGroup("0".to_string()));
            }
            FileAttr::SpaceUsed => {
                answer_attrs.push(FileAttr::SpaceUsed);
                attrs.push(FileAttrValue::SpaceUsed(4096));
            }
            FileAttr::TimeAccess => {
                answer_attrs.push(FileAttr::TimeAccess);
                attrs.push(FileAttrValue::TimeAccess(now));
            }
            FileAttr::TimeMetadata => {
                answer_attrs.push(FileAttr::TimeMetadata);
                attrs.push(FileAttrValue::TimeMetadata(now));
            }
            FileAttr::TimeModify => {
                answer_attrs.push(FileAttr::TimeModify);
                attrs.push(FileAttrValue::TimeModify(now));
            }
            FileAttr::Acl => {
                answer_attrs.push(FileAttr::Acl);
                attrs.push(FileAttrValue::Acl(vec![]));
            }
            FileAttr::AclSupport => {
                answer_attrs.push(FileAttr::AclSupport);
                attrs.push(FileAttrValue::AclSupport(0));
            }
            FileAttr::FsLocations => {
                answer_attrs.push(FileAttr::FsLocations);
                attrs.push(FileAttrValue::FsLocations(FsLocations4 {
                    fs_root: vec!["/".to_string()],
                    locations: vec![],
                }));
            }
            FileAttr::Maxfilesize => {
                answer_attrs.push(FileAttr::Maxfilesize);
                attrs.push(FileAttrValue::Maxfilesize(i64::MAX as u64));
            }
            FileAttr::Maxread => {
                answer_attrs.push(FileAttr::Maxread);
                attrs.push(FileAttrValue::Maxread(1048576));
            }
            FileAttr::Maxwrite => {
                answer_attrs.push(FileAttr::Maxwrite);
                attrs.push(FileAttrValue::Maxwrite(1048576));
            }
            FileAttr::Maxlink => {
                answer_attrs.push(FileAttr::Maxlink);
                attrs.push(FileAttrValue::Maxlink(32000));
            }
            FileAttr::Maxname => {
                answer_attrs.push(FileAttr::Maxname);
                attrs.push(FileAttrValue::Maxname(255));
            }
            FileAttr::Homogeneous => {
                answer_attrs.push(FileAttr::Homogeneous);
                attrs.push(FileAttrValue::Homogeneous(true));
            }
            FileAttr::NoTrunc => {
                answer_attrs.push(FileAttr::NoTrunc);
                attrs.push(FileAttrValue::NoTrunc(true));
            }
            FileAttr::Cansettime => {
                answer_attrs.push(FileAttr::Cansettime);
                attrs.push(FileAttrValue::Cansettime(true));
            }
            FileAttr::ChownRestricted => {
                answer_attrs.push(FileAttr::ChownRestricted);
                attrs.push(FileAttrValue::ChownRestricted(true));
            }
            _ => {}
        }
    }

    (answer_attrs, attrs)
}

/// Build READDIR entries for the pseudo-root — one entry per export.
pub async fn pseudo_readdir(
    export_manager: &ExportManagerHandle,
    attr_request: &[FileAttr],
    cookie: u64,
) -> (Vec<Entry4>, bool) {
    let exports = export_manager.list_exports().await;
    let mut entries = Vec::new();

    for (i, export) in exports.iter().enumerate() {
        let entry_cookie = (i + 3) as u64; // cookies 0,1,2 reserved per RFC
        if entry_cookie <= cookie {
            continue;
        }

        // Build the export root filehandle id for attrs
        let (answer_attrs, attr_vals) = pseudo_export_entry_attrs(attr_request, export.export_id);

        entries.push(Entry4 {
            name: export.name.clone(),
            cookie: entry_cookie,
            attrs: Fattr4 {
                attrmask: answer_attrs,
                attr_vals,
            },
            nextentry: None,
        });
    }

    let eof = true; // we always return all exports at once
    (entries, eof)
}

/// Attrs for an export entry shown in the pseudo-root READDIR.
fn pseudo_export_entry_attrs(
    attr_request: &[FileAttr],
    export_id: u8,
) -> (Attrlist4<FileAttr>, Attrlist4<FileAttrValue>) {
    let mut answer_attrs = Attrlist4::<FileAttr>::new(None);
    let mut attrs = Attrlist4::<FileAttrValue>::new(None);
    let now = current_time();

    for fileattr in attr_request {
        match fileattr {
            FileAttr::Type => {
                answer_attrs.push(FileAttr::Type);
                attrs.push(FileAttrValue::Type(NfsFtype4::Nf4dir));
            }
            FileAttr::Change => {
                answer_attrs.push(FileAttr::Change);
                attrs.push(FileAttrValue::Change(1));
            }
            FileAttr::Size => {
                answer_attrs.push(FileAttr::Size);
                attrs.push(FileAttrValue::Size(4096));
            }
            FileAttr::Fsid => {
                answer_attrs.push(FileAttr::Fsid);
                attrs.push(FileAttrValue::Fsid(Fsid4 {
                    major: export_id as u64,
                    minor: export_id as u64,
                }));
            }
            FileAttr::Fileid => {
                answer_attrs.push(FileAttr::Fileid);
                attrs.push(FileAttrValue::Fileid(export_id as u64 + 100));
            }
            FileAttr::Mode => {
                answer_attrs.push(FileAttr::Mode);
                attrs.push(FileAttrValue::Mode(0o755));
            }
            FileAttr::Numlinks => {
                answer_attrs.push(FileAttr::Numlinks);
                attrs.push(FileAttrValue::Numlinks(2));
            }
            FileAttr::Owner => {
                answer_attrs.push(FileAttr::Owner);
                attrs.push(FileAttrValue::Owner("0".to_string()));
            }
            FileAttr::OwnerGroup => {
                answer_attrs.push(FileAttr::OwnerGroup);
                attrs.push(FileAttrValue::OwnerGroup("0".to_string()));
            }
            FileAttr::SpaceUsed => {
                answer_attrs.push(FileAttr::SpaceUsed);
                attrs.push(FileAttrValue::SpaceUsed(4096));
            }
            FileAttr::TimeAccess => {
                answer_attrs.push(FileAttr::TimeAccess);
                attrs.push(FileAttrValue::TimeAccess(now));
            }
            FileAttr::TimeMetadata => {
                answer_attrs.push(FileAttr::TimeMetadata);
                attrs.push(FileAttrValue::TimeMetadata(now));
            }
            FileAttr::TimeModify => {
                answer_attrs.push(FileAttr::TimeModify);
                attrs.push(FileAttrValue::TimeModify(now));
            }
            FileAttr::RdattrError => {
                answer_attrs.push(FileAttr::RdattrError);
                attrs.push(FileAttrValue::RdattrError(NfsStat4::Nfs4errInval));
            }
            FileAttr::FhExpireType => {
                answer_attrs.push(FileAttr::FhExpireType);
                attrs.push(FileAttrValue::FhExpireType(
                    nextnfs_proto::nfs4_proto::FH4_PERSISTENT,
                ));
            }
            FileAttr::SupportedAttrs => {
                answer_attrs.push(FileAttr::SupportedAttrs);
                attrs.push(FileAttrValue::SupportedAttrs(pseudo_supported_attrs()));
            }
            FileAttr::UniqueHandles => {
                answer_attrs.push(FileAttr::UniqueHandles);
                attrs.push(FileAttrValue::UniqueHandles(true));
            }
            FileAttr::LeaseTime => {
                answer_attrs.push(FileAttr::LeaseTime);
                attrs.push(FileAttrValue::LeaseTime(90));
            }
            FileAttr::LinkSupport => {
                answer_attrs.push(FileAttr::LinkSupport);
                attrs.push(FileAttrValue::LinkSupport(false));
            }
            FileAttr::SymlinkSupport => {
                answer_attrs.push(FileAttr::SymlinkSupport);
                attrs.push(FileAttrValue::SymlinkSupport(false));
            }
            FileAttr::NamedAttr => {
                answer_attrs.push(FileAttr::NamedAttr);
                attrs.push(FileAttrValue::NamedAttr(true));
            }
            FileAttr::Acl => {
                answer_attrs.push(FileAttr::Acl);
                attrs.push(FileAttrValue::Acl(vec![]));
            }
            FileAttr::AclSupport => {
                answer_attrs.push(FileAttr::AclSupport);
                attrs.push(FileAttrValue::AclSupport(0));
            }
            FileAttr::FsLocations => {
                answer_attrs.push(FileAttr::FsLocations);
                attrs.push(FileAttrValue::FsLocations(FsLocations4 {
                    fs_root: vec!["/".to_string()],
                    locations: vec![],
                }));
            }
            FileAttr::Maxfilesize => {
                answer_attrs.push(FileAttr::Maxfilesize);
                attrs.push(FileAttrValue::Maxfilesize(i64::MAX as u64));
            }
            FileAttr::Maxread => {
                answer_attrs.push(FileAttr::Maxread);
                attrs.push(FileAttrValue::Maxread(1048576));
            }
            FileAttr::Maxwrite => {
                answer_attrs.push(FileAttr::Maxwrite);
                attrs.push(FileAttrValue::Maxwrite(1048576));
            }
            FileAttr::Maxlink => {
                answer_attrs.push(FileAttr::Maxlink);
                attrs.push(FileAttrValue::Maxlink(32000));
            }
            FileAttr::Maxname => {
                answer_attrs.push(FileAttr::Maxname);
                attrs.push(FileAttrValue::Maxname(255));
            }
            FileAttr::Homogeneous => {
                answer_attrs.push(FileAttr::Homogeneous);
                attrs.push(FileAttrValue::Homogeneous(true));
            }
            FileAttr::NoTrunc => {
                answer_attrs.push(FileAttr::NoTrunc);
                attrs.push(FileAttrValue::NoTrunc(true));
            }
            FileAttr::Cansettime => {
                answer_attrs.push(FileAttr::Cansettime);
                attrs.push(FileAttrValue::Cansettime(true));
            }
            FileAttr::ChownRestricted => {
                answer_attrs.push(FileAttr::ChownRestricted);
                attrs.push(FileAttrValue::ChownRestricted(true));
            }
            _ => {}
        }
    }

    (answer_attrs, attrs)
}

fn pseudo_supported_attrs() -> Attrlist4<FileAttr> {
    Attrlist4::<FileAttr>::new(Some(vec![
        FileAttr::SupportedAttrs,
        FileAttr::Type,
        FileAttr::FhExpireType,
        FileAttr::Change,
        FileAttr::Size,
        FileAttr::LinkSupport,
        FileAttr::SymlinkSupport,
        FileAttr::NamedAttr,
        FileAttr::Fsid,
        FileAttr::UniqueHandles,
        FileAttr::LeaseTime,
        FileAttr::RdattrError,
        FileAttr::AclSupport,
        FileAttr::Cansettime,
        FileAttr::ChownRestricted,
        FileAttr::Fileid,
        FileAttr::Homogeneous,
        FileAttr::Maxfilesize,
        FileAttr::Maxlink,
        FileAttr::Maxname,
        FileAttr::Maxread,
        FileAttr::Maxwrite,
        FileAttr::Mode,
        FileAttr::NoTrunc,
        FileAttr::Numlinks,
        FileAttr::Owner,
        FileAttr::OwnerGroup,
        FileAttr::SpaceUsed,
        FileAttr::TimeAccess,
        FileAttr::TimeMetadata,
        FileAttr::TimeModify,
    ]))
}

fn current_time() -> Nfstime4 {
    let since_epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    Nfstime4 {
        seconds: since_epoch.as_secs() as i64,
        nseconds: since_epoch.subsec_nanos(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pseudo_root_fh_structure() {
        let fh = pseudo_root_fh();
        assert_eq!(fh[0], 0x02);
        assert_eq!(fh[1], PSEUDO_ROOT_EXPORT_ID);
        // Rest should be zero
        for &byte in &fh[2..] {
            assert_eq!(byte, 0);
        }
    }

    #[test]
    fn test_is_pseudo_root() {
        let fh = pseudo_root_fh();
        assert!(is_pseudo_root(&fh));

        // Regular filehandle should not be pseudo-root
        let regular = [0u8; 26];
        assert!(!is_pseudo_root(&regular));
    }

    #[test]
    fn test_export_id_from_fh() {
        let pseudo_fh = pseudo_root_fh();
        assert_eq!(export_id_from_fh(&pseudo_fh), PSEUDO_ROOT_EXPORT_ID);

        // Regular fh returns fh[1]
        let mut regular = [0u8; 26];
        regular[1] = 5;
        assert_eq!(export_id_from_fh(&regular), 5);
    }

    #[test]
    fn test_stamp_export_id() {
        let mut fh = [0u8; 26];
        stamp_export_id(&mut fh, 42);
        assert_eq!(fh[1], 42);
    }

    #[test]
    fn test_pseudo_root_getattr_type() {
        let (answer_attrs, attrs) = pseudo_root_getattr(&[FileAttr::Type]);
        assert_eq!(answer_attrs.len(), 1);
        assert_eq!(attrs.len(), 1);
        assert_eq!(attrs[0], FileAttrValue::Type(NfsFtype4::Nf4dir));
    }

    #[test]
    fn test_pseudo_root_getattr_multiple() {
        let (answer_attrs, attrs) =
            pseudo_root_getattr(&[FileAttr::Type, FileAttr::Mode, FileAttr::Size]);
        assert_eq!(answer_attrs.len(), 3);
        assert_eq!(attrs.len(), 3);
    }
}
