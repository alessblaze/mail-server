use jmap_proto::types::{blob::BlobId, value::AclGrant};

pub struct FileNode {
    pub parent_id: Option<u32>,
    pub blob_id: Option<BlobId>,
    pub size: Option<u64>,
    pub name: String,
    pub media_type: Option<String>,
    pub executable: bool,
    pub created: u64,
    pub modified: u64,
    pub acls: Vec<AclGrant>,
}
