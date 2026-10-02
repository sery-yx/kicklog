use tmi::Tag;

/// Access to the tags of a parsed IRC line, used by the importer for raw logs
pub trait MessageWithTags {
    fn get_tag(&self, key: Tag) -> Option<&str>;
}

impl MessageWithTags for tmi::IrcMessageRef<'_> {
    fn get_tag(&self, key: Tag) -> Option<&str> {
        self.tag(key)
    }
}

pub fn extract_user_id<T: MessageWithTags>(msg: &T) -> Option<&str> {
    msg.get_tag(Tag::UserId)
        .or_else(|| msg.get_tag(Tag::TargetUserId))
}

pub fn extract_raw_timestamp<T: MessageWithTags>(msg: &T) -> Option<u64> {
    msg.get_tag(Tag::TmiSentTs)
        .and_then(|raw_timestamp| raw_timestamp.parse().ok())
}
