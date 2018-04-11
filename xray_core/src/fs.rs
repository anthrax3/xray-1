use futures::{Async, Future, Stream};
use notify_cell::NotifyCell;
use parking_lot::RwLock;
use rpc::{client, server};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
#[cfg(test)]
use serde_json;
use std::cell::RefCell;
use std::ffi::{OsStr, OsString};
use std::iter::Iterator;
use std::path::Path;
use std::rc::Rc;
use std::result;
use std::sync::Arc;
use ForegroundExecutor;

pub type EntryId = usize;
pub type Result<T> = result::Result<T, ()>;

pub trait Tree {
    fn path(&self) -> &Path;
    fn root(&self) -> Entry;
    fn updates(&self) -> Box<Stream<Item = (), Error = ()>>;

    // Returns a promise that resolves once tree is populated
    // We could potentially implement this promise from an observer for a boolean notify cell
    // to avoid needing to maintain a set of oneshot channels or something similar.
    // cell.observe().skip_while(|resolved| !resolved).into_future().then(Ok(()))
    fn populated(&self) -> Box<Future<Item = (), Error = ()>>;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Entry {
    #[serde(serialize_with = "serialize_dir", deserialize_with = "deserialize_dir")]
    Dir(Arc<DirInner>),
    #[serde(serialize_with = "serialize_file", deserialize_with = "deserialize_file")]
    File(Arc<FileInner>),
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DirInner {
    name: OsString,
    #[serde(skip_serializing, skip_deserializing)]
    name_chars: Vec<char>,
    #[serde(serialize_with = "serialize_dir_children")]
    #[serde(deserialize_with = "deserialize_dir_children")]
    children: RwLock<Arc<Vec<Entry>>>,
    symlink: bool,
    ignored: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FileInner {
    name: OsString,
    name_chars: Vec<char>,
    symlink: bool,
    ignored: bool,
}

pub struct TreeService {
    tree: Rc<Tree>,
    populated: Option<Box<Future<Item = (), Error = ()>>>,
}

pub struct RemoteTree(Rc<RefCell<RemoteTreeState>>);

struct RemoteTreeState {
    root: Entry,
    updates: NotifyCell<()>,
}

impl Entry {
    pub fn file(name: OsString, symlink: bool, ignored: bool) -> Self {
        Entry::File(Arc::new(FileInner {
            name_chars: name.to_string_lossy().chars().collect(),
            name,
            symlink,
            ignored,
        }))
    }

    pub fn dir(name: OsString, symlink: bool, ignored: bool) -> Self {
        let mut name_chars: Vec<char> = name.to_string_lossy().chars().collect();
        name_chars.push('/');
        Entry::Dir(Arc::new(DirInner {
            name_chars,
            name,
            children: RwLock::new(Arc::new(Vec::new())),
            symlink,
            ignored,
        }))
    }

    pub fn is_dir(&self) -> bool {
        match self {
            &Entry::Dir(_) => true,
            &Entry::File(_) => false,
        }
    }

    pub fn id(&self) -> EntryId {
        match self {
            &Entry::Dir(ref inner) => inner.as_ref() as *const DirInner as EntryId,
            &Entry::File(ref inner) => inner.as_ref() as *const FileInner as EntryId,
        }
    }

    pub fn name(&self) -> &OsStr {
        match self {
            &Entry::Dir(ref inner) => &inner.name,
            &Entry::File(ref inner) => &inner.name,
        }
    }

    pub fn name_chars(&self) -> &[char] {
        match self {
            &Entry::Dir(ref inner) => &inner.name_chars,
            &Entry::File(ref inner) => &inner.name_chars,
        }
    }

    pub fn is_symlink(&self) -> bool {
        match self {
            &Entry::Dir(ref inner) => inner.symlink,
            &Entry::File(ref inner) => inner.symlink,
        }
    }

    pub fn is_ignored(&self) -> bool {
        match self {
            &Entry::Dir(ref inner) => inner.ignored,
            &Entry::File(ref inner) => inner.ignored,
        }
    }

    pub fn children(&self) -> Option<Arc<Vec<Entry>>> {
        match self {
            &Entry::Dir(ref inner) => Some(inner.children.read().clone()),
            &Entry::File(..) => None,
        }
    }

    pub fn insert(&self, new_entry: Entry) -> Result<()> {
        match self {
            &Entry::Dir(ref inner) => {
                let mut children = inner.children.write();
                let children = Arc::make_mut(&mut children);
                if children
                    .last()
                    .map(|child| child.name() < new_entry.name())
                    .unwrap_or(true)
                {
                    children.push(new_entry);
                    Ok(())
                } else {
                    let index = {
                        let new_name = new_entry.name();
                        match children.binary_search_by(|child| child.name().cmp(new_name)) {
                            Ok(_) => return Err(()), // An entry already exists with this name
                            Err(index) => index,
                        }
                    };
                    children.insert(index, new_entry);
                    Ok(())
                }
            }
            &Entry::File(_) => Err(()),
        }
    }
}

fn serialize_dir<S: Serializer>(
    dir: &Arc<DirInner>,
    serializer: S,
) -> result::Result<S::Ok, S::Error> {
    dir.serialize(serializer)
}

fn deserialize_dir<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> result::Result<Arc<DirInner>, D::Error> {
    let mut inner = DirInner::deserialize(deserializer)?;

    let mut name_chars: Vec<char> = inner.name.to_string_lossy().chars().collect();
    name_chars.push('/');
    inner.name_chars = name_chars;

    Ok(Arc::new(inner))
}

fn serialize_file<S: Serializer>(
    file: &Arc<FileInner>,
    serializer: S,
) -> result::Result<S::Ok, S::Error> {
    file.serialize(serializer)
}

fn deserialize_file<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> result::Result<Arc<FileInner>, D::Error> {
    let mut inner = FileInner::deserialize(deserializer)?;
    inner.name_chars = inner.name.to_string_lossy().chars().collect();
    Ok(Arc::new(inner))
}

fn serialize_dir_children<S: Serializer>(
    children: &RwLock<Arc<Vec<Entry>>>,
    serializer: S,
) -> result::Result<S::Ok, S::Error> {
    children.read().serialize(serializer)
}

fn deserialize_dir_children<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> result::Result<RwLock<Arc<Vec<Entry>>>, D::Error> {
    Ok(RwLock::new(Arc::new(Vec::deserialize(deserializer)?)))
}

impl TreeService {
    pub fn new(tree: Rc<Tree>) -> Self {
        let populated = Some(tree.populated());
        Self { tree, populated }
    }
}

impl server::Service for TreeService {
    type State = Entry;
    type Update = Entry;
    type Request = ();
    type Response = ();

    fn state(&self, _: &server::Connection) -> Self::State {
        let root = self.tree.root();
        Entry::dir(root.name().to_owned(), root.is_symlink(), root.is_ignored())
    }

    fn poll_update(&mut self, _: &server::Connection) -> Async<Option<Self::Update>> {
        if let Some(populated) = self.populated.as_mut().map(|p| p.poll().unwrap()) {
            if let Async::Ready(_) = populated {
                self.populated.take();
                Async::Ready(Some(self.tree.root().clone()))
            } else {
                Async::NotReady
            }
        } else {
            Async::NotReady
        }
    }
}

impl RemoteTree {
    pub fn new(foreground: ForegroundExecutor, client: client::Service<TreeService>) -> Self {
        let state = Rc::new(RefCell::new(RemoteTreeState {
            root: client.state().unwrap(),
            updates: NotifyCell::new(()),
        }));

        let state_clone = state.clone();
        foreground
            .execute(Box::new(client.updates().unwrap().for_each(move |root| {
                let mut state = state_clone.borrow_mut();
                state.root = root;
                state.updates.set(());
                Ok(())
            })))
            .unwrap();

        RemoteTree(state)
    }
}

impl Tree for RemoteTree {
    fn path(&self) -> &Path {
        unimplemented!()
    }

    fn root(&self) -> Entry {
        self.0.borrow().root.clone()
    }

    fn updates(&self) -> Box<Stream<Item = (), Error = ()>> {
        Box::new(self.0.borrow().updates.observe())
    }

    fn populated(&self) -> Box<Future<Item = (), Error = ()>> {
        unimplemented!()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use bincode::{deserialize, serialize};
    use notify_cell::NotifyCell;
    use rpc;
    use std::path::PathBuf;
    use stream_ext::StreamExt;
    use tokio_core::reactor;

    #[test]
    fn test_insert() {
        let root = Entry::dir(OsString::from("root"), false, false);
        assert_eq!(
            root.insert(Entry::file(OsString::from("a"), false, false)),
            Ok(())
        );
        assert_eq!(
            root.insert(Entry::file(OsString::from("c"), false, false)),
            Ok(())
        );
        assert_eq!(
            root.insert(Entry::file(OsString::from("b"), false, false)),
            Ok(())
        );
        assert_eq!(
            root.insert(Entry::file(OsString::from("a"), false, false)),
            Err(())
        );
        assert_eq!(root.child_names(), vec!["a", "b", "c"]);
    }

    #[test]
    fn test_serialize_deserialize() {
        let root = Entry::from_json(
            "root",
            &json!({
                "child-1": {
                    "subchild-1-1": null
                },
                "child-2": null,
                "child-3": {
                    "subchild-3-1": {
                        "subchild-3-1-1": null,
                        "subchild-3-1-2": null,
                    }
                }
            }),
        );
        assert_eq!(
            deserialize::<Entry>(&serialize(&root).unwrap()).unwrap(),
            root
        );
    }

    #[test]
    fn test_tree_replication() {
        let mut reactor = reactor::Core::new().unwrap();
        let handle = Rc::new(reactor.handle());

        let local_tree = Rc::new(TestTree::new(
            "/foo/bar",
            Entry::from_json(
                "root",
                &json!({
                    "child-1": {
                        "subchild": null
                    },
                    "child-2": null,
                }),
            ),
        ));
        let remote_tree = RemoteTree::new(
            handle,
            rpc::tests::connect(&mut reactor, TreeService::new(local_tree.clone())),
        );
        assert_eq!(remote_tree.root().name(), local_tree.root().name());
        assert_eq!(remote_tree.root().children().unwrap().len(), 0);

        let mut remote_tree_updates = remote_tree.updates();
        local_tree.populated.set(true);
        remote_tree_updates.wait_next(&mut reactor);
        assert_eq!(remote_tree.root(), local_tree.root());
    }

    pub struct TestTree {
        path: PathBuf,
        root: Entry,
        populated: NotifyCell<bool>,
    }

    impl TestTree {
        pub fn new<T: Into<PathBuf>>(path: T, root: Entry) -> Self {
            Self {
                path: path.into(),
                root,
                populated: NotifyCell::new(false),
            }
        }

        pub fn from_json<T: Into<PathBuf>>(path: T, json: serde_json::Value) -> Self {
            let path = path.into();
            let root = Entry::from_json(path.file_name().unwrap(), &json);
            Self::new(path, root)
        }
    }

    impl Tree for TestTree {
        fn path(&self) -> &Path {
            &self.path
        }

        fn root(&self) -> Entry {
            self.root.clone()
        }

        fn updates(&self) -> Box<Stream<Item = (), Error = ()>> {
            unimplemented!()
        }

        fn populated(&self) -> Box<Future<Item = (), Error = ()>> {
            Box::new(
                self.populated
                    .observe()
                    .skip_while(|p| Ok(!p))
                    .into_future()
                    .then(|_| Ok(())),
            )
        }
    }

    impl Entry {
        fn from_json<T: Into<OsString>>(name: T, json: &serde_json::Value) -> Self {
            if json.is_object() {
                let object = json.as_object().unwrap();
                let dir = Entry::dir(name.into(), false, false);
                for (key, value) in object {
                    let child_entry = Self::from_json(key, value);
                    assert_eq!(dir.insert(child_entry), Ok(()));
                }
                dir
            } else {
                Entry::file(name.into(), false, false)
            }
        }

        fn child_names(&self) -> Vec<String> {
            match self {
                &Entry::Dir(ref inner) => inner
                    .children
                    .read()
                    .iter()
                    .map(|ref entry| entry.name().to_string_lossy().into_owned())
                    .collect(),
                _ => panic!(),
            }
        }
    }

    impl PartialEq for Entry {
        fn eq(&self, other: &Self) -> bool {
            self.name() == other.name() && self.name_chars() == other.name_chars()
                && self.is_dir() == other.is_dir()
                && self.is_ignored() == other.is_ignored()
                && self.children() == other.children()
        }
    }
}
