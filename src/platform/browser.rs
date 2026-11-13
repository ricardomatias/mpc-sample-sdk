//! The stock Sample Browser (SAMPLE SELECT): what it is showing, and a way to
//! change the files it lists.
//!
//! The browser is the app's `FileSelectScreen`.  Its `ui::FileListBox` shows
//! the files of one `AC50DirectoryContents` at a time, one per location
//! (Internal, External).  Every time a folder is opened or changes, the
//! contents object builds a `std::vector<juce::File>` (`0x14f8d60`) and hands
//! it to the list box.  The SDK hooks that builder: it remembers the contents
//! object and lets the mod replace the vector before the list box sees it
//! ([`crate::Mod::on_browser_list`]).  [`refresh`] runs the stock refresh, so a
//! mod can change its mind and have the browser list again.
//!
//! The list box shows the vector, but it asks the contents object for the file
//! in a row (`getFile`, `0x14f88f0`) when it previews or loads one, and the
//! stock answer comes from the folder's own directory list.  The SDK hooks
//! that lookup as well: while a contents object shows a replaced listing, row
//! `n` is the replacement's `n`th path, so preview and load follow what is on
//! screen.
//!
//! The same classes serve the project browser, so every reading says which
//! [`Kind`] of listing it is: a contents object's root folder is `.../Samples`
//! or `.../Projects`, or, on a card, the card itself (`/media/MPC SAMPLE`),
//! and then the folder being shown tells (`MPC-Sample/Samples`).
//!
//! The browser's state comes from the app's own objects: the screen is open
//! when its JUCE visible flag is set, the folder is the contents object's
//! current directory, and the cursor is the row of the list box's model.  The
//! SDK maps that row to a path through the last listing it saw that contents
//! object build, since the list box shows the vector in order.
//!
//! A `juce::File` is one `juce::String`: a pointer to UTF-8 text with the
//! string's reference count 16 bytes in front of it.  The SDK reads them as
//! paths and builds new ones with the app's own `String(const char *)`.

use core::sync::atomic::{AtomicUsize, Ordering};
use std::ffi::{CString, OsStr};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use crate::image;

/// `AC50DirectoryContents`'s vtable (its address point), from the image base.
pub const CONTENTS_VTABLE: usize = 0x6d8_cb40;
/// `FileSelectScreen`'s primary vtable (its address point), from the image base.
pub const SCREEN_VTABLE: usize = 0x6e1_cfb8;
/// `AC50DirectoryContents::buildFileList`: `(this) -> std::vector<juce::File>`.
pub const BUILD_LIST: usize = 0x14f_8d60;
/// `AC50DirectoryContents`'s refresh: builds the list and hands it to its
/// listeners (the list box).  Vtable slot 2; slots 3 and 5 are the same code.
pub const REFRESH: usize = 0x14f_95c0;
/// `FileSelectScreen`'s constructor: `(this, locator)`.
pub const SCREEN_CTOR: usize = 0x160_bbf0;
/// `AC50DirectoryContents::getFile`: `(this, row) -> std::optional<juce::File>`,
/// returned through `x8`.
pub const GET_FILE: usize = 0x14f_88f0;
/// The app's `juce::String(const char *)`.
const STRING_FROM_C: usize = 0x0f5_35c0;
/// The app's `juce::String::~String` (and `juce::File::~File`).
const STRING_DROP: usize = 0x0f0_a290;

/// `[contents + 0x30]` is the stock `DirectoryContents` it wraps.
const CONTENTS_LIST: usize = 0x30;
/// `contents + 0x38` is its root folder (a `juce::File`): the stock builder
/// appends the `/data` user folder when it lists this one.
const CONTENTS_ROOT: usize = 0x38;
/// `[list + 0x4a0]` is the folder the list is showing (a `juce::File`).
const LIST_FOLDER: usize = 0x4a0;
/// `screen + 0xff0` is the screen's `ui::FileListBox` ("fileListBox").
const SCREEN_LIST_BOX: usize = 0xff0;
/// `[list box + 0x188]` is the contents object it shows.
const LIST_BOX_CONTENTS: usize = 0x188;
/// `[list box + 0xc0]` is its `ui::ListBoxModel`.
const LIST_BOX_MODEL: usize = 0xc0;
/// `[model + 0x98]` is the row the cursor is on (an `i32`).
const MODEL_ROW: usize = 0x98;
/// The object at `[list box + 0xc0]` is also the component that holds every
/// row; its bounds (x, y, width, height as `i32`s) start at `+0xd0`, so `+0xd4`
/// is where its top sits in the visible part of the list: zero at the top of
/// the list and negative once the list has scrolled down.
const MODEL_TOP: usize = 0xd4;
/// `screen + 0xa8` holds the JUCE `Component` flags of the screen.
const SCREEN_FLAGS: usize = 0xa8;
/// `screen + 0x9d8` is 1 while the auto-preview (SCREEN 3, "Preview") is on
/// and 0 while it is off. Found by toggling it in the stock app and comparing
/// the screen's bytes; `+0xa2c` follows it too. It keeps its value while the
/// browser is closed.
const SCREEN_PREVIEW: usize = 0x9d8;
/// `ComponentFlags::visibleFlag`.
const VISIBLE_FLAG: u8 = 0x02;
/// Contents objects the app builds: Internal and External, for samples and
/// for projects.  More than this means the bookkeeping is wrong.
const MAX_CONTENTS: usize = 8;

/// The longest path the SDK reads out of a `juce::String`.
const MAX_PATH: usize = 1024;
/// Most files one listing may hold; anything bigger is not a vector of files.
const MAX_FILES: usize = 100_000;

/// The live `FileSelectScreen`, zero until the app has built it.
static SCREEN: AtomicUsize = AtomicUsize::new(0);
/// The `AC50DirectoryContents` that last built a listing.
static CONTENTS: AtomicUsize = AtomicUsize::new(0);
/// The last listing each contents object handed its list box, as shown.
static SHOWN: Mutex<Shown> = Mutex::new(Shown::new());

/// What kind of files a listing holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// The Sample Browser (a root folder named `Samples`).
    Samples,
    /// The project browser (a root folder named `Projects`).
    Projects,
    /// Any other root.
    Other,
}

impl Kind {
    /// The kind of listing a contents object with this root folder makes.
    #[must_use]
    pub fn of_root(root: &Path) -> Self {
        match root.file_name().map(OsStr::as_bytes) {
            Some(b"Samples") => Self::Samples,
            Some(b"Projects") => Self::Projects,
            _ => Self::Other,
        }
    }

    /// The kind of listing a contents object makes, from its root folder and
    /// the folder it is showing.
    ///
    /// Internal roots are `.../Samples` and `.../Projects` and say it
    /// themselves.  A card's root is the card (`/media/MPC SAMPLE`), and the
    /// browser opens in its `MPC-Sample/Samples` or `MPC-Sample/Projects`, so
    /// for a root that says nothing the first folder below it (after an
    /// `MPC-Sample` folder) does.
    #[must_use]
    pub fn of(root: &Path, folder: Option<&Path>) -> Self {
        let named = Self::of_root(root);
        if named != Self::Other {
            return named;
        }
        let Some(inside) = folder.and_then(|folder| folder.strip_prefix(root).ok()) else {
            return Self::Other;
        };
        let mut parts = inside.components().map(|part| part.as_os_str().as_bytes());
        match (parts.next(), parts.next()) {
            (Some(b"MPC-Sample"), Some(name)) | (Some(name), _) => match name {
                b"Samples" => Self::Samples,
                b"Projects" => Self::Projects,
                _ => Self::Other,
            },
            (None, _) => Self::Other,
        }
    }
}

/// The paths each contents object last showed, keyed by its address, and
/// whether a mod replaced them.
struct Shown {
    entries: Vec<(usize, Vec<PathBuf>, bool)>,
}

impl Shown {
    const fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// Record what `contents` shows now.
    fn record(&mut self, contents: usize, files: Vec<PathBuf>, replaced: bool) {
        if let Some(entry) = self.entries.iter_mut().find(|entry| entry.0 == contents) {
            entry.1 = files;
            entry.2 = replaced;
            return;
        }
        if self.entries.len() >= MAX_CONTENTS {
            self.entries.remove(0);
        }
        self.entries.push((contents, files, replaced));
    }

    /// Every path `contents` shows, in row order.
    fn files(&self, contents: usize) -> Vec<PathBuf> {
        self.entries
            .iter()
            .find(|entry| entry.0 == contents)
            .map_or_else(Vec::new, |entry| entry.1.clone())
    }

    /// Whether what `contents` shows is a mod's replacement.
    fn is_replaced(&self, contents: usize) -> bool {
        self.entries
            .iter()
            .any(|entry| entry.0 == contents && entry.2)
    }

    /// The path in row `row` of what `contents` shows.
    fn row(&self, contents: usize, row: i32) -> Option<PathBuf> {
        let files = &self.entries.iter().find(|entry| entry.0 == contents)?.1;
        files.get(usize::try_from(row).ok()?).cloned()
    }

    /// Who answers for row `row` of what `contents` shows.
    fn replaced_row(&self, contents: usize, row: i32) -> Row {
        let Some(entry) = self.entries.iter().find(|entry| entry.0 == contents) else {
            return Row::Stock;
        };
        if !entry.2 {
            return Row::Stock;
        }
        usize::try_from(row)
            .ok()
            .and_then(|row| entry.1.get(row).cloned())
            .map_or(Row::Empty, Row::Path)
    }
}

/// Which of the browser's locations a folder belongs to, as the browser's own
/// SCREEN 2 tab names them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Location {
    /// The MPC Sample's own storage (`/data`, and the factory library).
    Internal,
    /// The card (`/media/...`).
    External,
}

impl Location {
    /// The location a path lives in.
    #[must_use]
    pub fn of(path: &Path) -> Self {
        if path.starts_with("/media") {
            Self::External
        } else {
            Self::Internal
        }
    }
}

/// What the Sample Browser is showing, read in one go.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct State {
    /// The folder the list is showing.
    pub folder: PathBuf,
    /// The location that folder is in.
    pub location: Location,
    /// The file or folder the cursor is on, if the list is not empty.
    pub highlighted: Option<PathBuf>,
}

/// One listing the browser is about to show.
///
/// The SDK hands it to [`crate::Mod::on_browser_list`] after the app built it.
/// A mod reads [`Self::folder`] and [`Self::files`] and may call
/// [`Self::replace`]; the browser then lists exactly those paths, in that
/// order, in place of the stock ones.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Listing {
    kind: Option<Kind>,
    folder: Option<PathBuf>,
    files: Vec<PathBuf>,
    replacement: Option<Vec<PathBuf>>,
}

impl Listing {
    /// A listing of `files` in `folder`, as the app built it.
    #[must_use]
    pub fn new(kind: Option<Kind>, folder: Option<PathBuf>, files: Vec<PathBuf>) -> Self {
        Self {
            kind,
            folder,
            files,
            replacement: None,
        }
    }

    /// Whether this is a sample listing, a project listing or another; `None`
    /// when the root folder could not be read.
    #[must_use]
    pub fn kind(&self) -> Option<Kind> {
        self.kind
    }

    /// The folder the browser is listing, when it could be read.
    #[must_use]
    pub fn folder(&self) -> Option<&Path> {
        self.folder.as_deref()
    }

    /// The location of that folder.
    #[must_use]
    pub fn location(&self) -> Option<Location> {
        self.folder().map(Location::of)
    }

    /// The files and folders the app listed, in its order.
    #[must_use]
    pub fn files(&self) -> &[PathBuf] {
        &self.files
    }

    /// List these paths instead.  They may come from any folder or location;
    /// each must be an absolute path.  Paths the app could not represent (not
    /// absolute, a trailing `/`, or a NUL byte) are dropped.
    pub fn replace(&mut self, files: Vec<PathBuf>) {
        self.replacement = Some(files.into_iter().filter(|path| is_listable(path)).collect());
    }

    /// The paths the mod asked for, if it replaced the listing.
    #[must_use]
    pub fn replacement(&self) -> Option<&[PathBuf]> {
        self.replacement.as_deref()
    }

    /// Take the replacement out, leaving the listing as the app built it.
    pub(crate) fn take_replacement(&mut self) -> Option<Vec<PathBuf>> {
        self.replacement.take()
    }

    /// The stock files, taken out of the listing.
    pub(crate) fn into_files(self) -> Vec<PathBuf> {
        self.files
    }
}

/// Whether the app can show `path` as a list entry: an absolute, normalised
/// path, which is how a `juce::File` stores itself.
#[must_use]
pub fn is_listable(path: &Path) -> bool {
    let bytes = path.as_os_str().as_bytes();
    path.is_absolute()
        && bytes.len() > 1
        && bytes.len() < MAX_PATH
        && !bytes.ends_with(b"/")
        && !bytes.contains(&0)
        && !bytes.windows(2).any(|pair| pair == b"//")
}

/// Remember the browser screen once its constructor has run.
pub(crate) fn remember_screen(screen: usize) {
    SCREEN.store(screen, Ordering::Release);
}

/// Remember the contents object that just built a listing, what it shows, and
/// whether that is a mod's replacement.
pub(crate) fn remember_listing(contents: usize, shown: Vec<PathBuf>, replaced: bool) {
    CONTENTS.store(contents, Ordering::Release);
    SHOWN
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .record(contents, shown, replaced);
}

/// Who answers for a row of a contents object's listing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Row {
    /// The stock listing is showing: the app answers from the folder.
    Stock,
    /// The replaced listing shows this path in the row.
    Path(PathBuf),
    /// The replaced listing has no such row.
    Empty,
}

/// Who answers for row `row` of what `contents` shows.
pub(crate) fn replaced_row(contents: usize, row: i32) -> Row {
    SHOWN
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .replaced_row(contents, row)
}

/// `std::optional<juce::File>` as the app returns it.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OptionalFile {
    /// The file's `juce::String` text pointer, when engaged.
    pub file: usize,
    /// Whether a file is present.
    pub engaged: u8,
}

/// Fill an uninitialised `std::optional<juce::File>` with `path`, or leave it
/// empty.
///
/// # Safety
///
/// `out` must point to the app's result slot for one `getFile` call; the app
/// owns and destroys what is written there.
pub(crate) unsafe fn write_file(out: *mut OptionalFile, path: Option<&Path>) {
    let base = image::base();
    let text = path.and_then(|path| CString::new(path.as_os_str().as_bytes()).ok());
    // SAFETY: the caller hands the app's result slot.
    unsafe { (*out).engaged = 0 };
    let Some(text) = text else {
        return;
    };
    if base == 0 {
        return;
    }
    let make: unsafe extern "C" fn(*mut usize, *const libc::c_char) =
        // SAFETY: `String(const char *)` of the verified firmware.
        unsafe { core::mem::transmute::<usize, _>(base + STRING_FROM_C) };
    // SAFETY: the slot's first word is the `juce::File` the app will destroy.
    unsafe {
        make(&raw mut (*out).file, text.as_ptr());
        (*out).engaged = 1;
    }
}

/// The live `FileSelectScreen`, or zero before the app built it.
#[must_use]
pub fn screen() -> usize {
    SCREEN.load(Ordering::Acquire)
}

/// The contents object the browser's list box is showing, or `None`.
///
/// Falls back to the contents object that last built a listing when the
/// screen is not known yet.
#[must_use]
pub fn contents() -> Option<usize> {
    let screen = screen();
    if screen != 0
        && let Some(contents) = read_contents(screen)
    {
        return Some(contents);
    }
    let last = CONTENTS.load(Ordering::Acquire);
    is_contents(last).then_some(last)
}

/// The contents object behind a screen's list box.
fn read_contents(screen: usize) -> Option<usize> {
    if image::read_pointer(screen)? != image::base() + SCREEN_VTABLE {
        return None;
    }
    let contents = image::read_pointer(screen + SCREEN_LIST_BOX + LIST_BOX_CONTENTS)?;
    is_contents(contents).then_some(contents)
}

/// Whether `address` is an `AC50DirectoryContents`.
fn is_contents(address: usize) -> bool {
    address != 0
        && image::read_pointer(address)
            .is_some_and(|vtable| vtable == image::base() + CONTENTS_VTABLE)
}

/// The root folder of a contents object (`/content/Samples`, the card's
/// `MPC-Sample/Samples`, ...).
#[must_use]
pub fn root_of(contents: usize) -> Option<PathBuf> {
    read_path(image::read_pointer(contents.checked_add(CONTENTS_ROOT)?)?)
}

/// What kind of listing a contents object makes.
#[must_use]
pub fn kind_of(contents: usize) -> Option<Kind> {
    root_of(contents).map(|root| Kind::of(&root, folder_of(contents).as_deref()))
}

/// The folder a contents object is showing.
#[must_use]
pub fn folder_of(contents: usize) -> Option<PathBuf> {
    let list = image::read_pointer(contents.checked_add(CONTENTS_LIST)?)?;
    let text = image::read_pointer(list.checked_add(LIST_FOLDER)?)?;
    read_path(text)
}

/// The folder the browser is showing, or `None` before it first listed.
#[must_use]
pub fn folder() -> Option<PathBuf> {
    folder_of(contents()?)
}

/// The location the browser is showing.
#[must_use]
pub fn location() -> Option<Location> {
    folder().map(|folder| Location::of(&folder))
}

/// Whether the browser screen is up (samples or projects).
#[must_use]
pub fn screen_visible() -> bool {
    let screen = screen();
    if screen == 0 || image::read_pointer(screen) != Some(image::base() + SCREEN_VTABLE) {
        return false;
    }
    let mut flags = [0_u8];
    image::read(screen + SCREEN_FLAGS, &mut flags) && visible(flags[0])
}

/// Whether the browser's auto-preview is on, or `None` before the app built
/// the browser screen.
#[must_use]
pub fn auto_preview() -> Option<bool> {
    let screen = screen();
    if screen == 0 || image::read_pointer(screen) != Some(image::base() + SCREEN_VTABLE) {
        return None;
    }
    let mut flag = [0_u8];
    if !image::read(screen + SCREEN_PREVIEW, &mut flag) {
        return None;
    }
    match flag[0] {
        0 => Some(false),
        1 => Some(true),
        _ => None,
    }
}

/// Whether a JUCE flags byte has the visible flag set.
fn visible(flags: u8) -> bool {
    flags & VISIBLE_FLAG != 0
}

/// Whether the Sample Browser is open: the browser screen is up and listing
/// samples.
#[must_use]
pub fn is_open() -> bool {
    screen_visible() && contents().and_then(kind_of) == Some(Kind::Samples)
}

/// The row the cursor is on, or `None` when it cannot be read.
#[must_use]
pub fn cursor_row() -> Option<i32> {
    let screen = screen();
    if screen == 0 {
        return None;
    }
    let model = image::read_pointer(screen + SCREEN_LIST_BOX + LIST_BOX_MODEL)?;
    let row = image::read_u32(model.checked_add(MODEL_ROW)?)?;
    Some(row.cast_signed())
}

/// How far the list has scrolled, in pixels: zero while the first row is at
/// the top, more as the list moves up.  `None` when it cannot be read.
///
/// Row `n` of a list whose rows are `h` pixels tall starts `n * h - scrolled`
/// pixels below the top of the list, so a mod that draws on the browser can
/// tell which rows are on screen and where.
#[must_use]
pub fn scrolled() -> Option<i32> {
    let screen = screen();
    if screen == 0 {
        return None;
    }
    let rows = image::read_pointer(screen + SCREEN_LIST_BOX + LIST_BOX_MODEL)?;
    let top = image::read_u32(rows.checked_add(MODEL_TOP)?)?.cast_signed();
    Some(top.saturating_neg().max(0))
}

/// Every file and folder the list box shows, in row order (after any
/// replacement).  Empty before the browser has listed anything.
#[must_use]
pub fn listed() -> Vec<PathBuf> {
    contents().map_or_else(Vec::new, |contents| {
        SHOWN
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .files(contents)
    })
}

/// Whether the list box shows a listing that a mod replaced.
///
/// The app builds each location's listing when it opens a folder or is asked
/// to refresh, so a location can still show a listing from before a mod
/// changed its mind.  Comparing this and [`listed`] with what the mod wants
/// tells it to call [`refresh`].  A listing is also stock when the mod could
/// not be reached while the app built it (another callback held the module).
#[must_use]
pub fn is_replaced() -> bool {
    contents().is_some_and(|contents| {
        SHOWN
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_replaced(contents)
    })
}

/// The file or folder the cursor is on, as listed (after any replacement).
#[must_use]
pub fn highlighted() -> Option<PathBuf> {
    let contents = contents()?;
    let row = cursor_row()?;
    SHOWN
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .row(contents, row)
}

/// The Sample Browser's state, or `None` while it is closed.
#[must_use]
pub fn state() -> Option<State> {
    if !is_open() {
        return None;
    }
    let folder = folder()?;
    Some(State {
        location: Location::of(&folder),
        folder,
        highlighted: highlighted(),
    })
}

/// Read a `juce::String`'s text (the pointer a `juce::File` holds) as a path.
#[must_use]
pub fn read_path(text: usize) -> Option<PathBuf> {
    if text == 0 {
        return None;
    }
    let mut bytes = vec![0_u8; MAX_PATH];
    let mut read = 0;
    // Read in small steps: the string may end close to the end of a mapping.
    while read < MAX_PATH {
        let step = 64.min(MAX_PATH - read);
        if !image::read(text + read, &mut bytes[read..read + step]) {
            return None;
        }
        if let Some(end) = bytes[read..read + step].iter().position(|byte| *byte == 0) {
            bytes.truncate(read + end);
            return path_from_bytes(&bytes);
        }
        read += step;
    }
    None
}

/// A path from the bytes of a `juce::String`, when they name an absolute path.
fn path_from_bytes(bytes: &[u8]) -> Option<PathBuf> {
    let path = PathBuf::from(OsStr::from_bytes(bytes));
    path.is_absolute().then_some(path)
}

/// `std::vector<juce::File>` as the app passes it: begin, end, capacity end.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RawFiles {
    /// First `juce::File`.
    pub begin: usize,
    /// One past the last.
    pub end: usize,
    /// One past the allocation.
    pub capacity: usize,
}

impl RawFiles {
    /// The number of files, when the three pointers describe a vector.
    #[must_use]
    pub fn len(&self) -> Option<usize> {
        files_in(self.begin, self.end, self.capacity)
    }

    /// Whether the vector is empty or unreadable.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len().is_none_or(|count| count == 0)
    }
}

/// How many 8-byte files `begin..end` holds inside `begin..capacity`.
fn files_in(begin: usize, end: usize, capacity: usize) -> Option<usize> {
    if begin == 0 {
        return (end == 0 && capacity == 0).then_some(0);
    }
    if end < begin
        || capacity < end
        || !(end - begin).is_multiple_of(8)
        || !(capacity - begin).is_multiple_of(8)
    {
        return None;
    }
    let count = (end - begin) / 8;
    (count <= MAX_FILES).then_some(count)
}

/// Read every path out of a vector the app built.
#[must_use]
pub fn read_files(files: &RawFiles) -> Vec<PathBuf> {
    let Some(count) = files.len() else {
        return Vec::new();
    };
    (0..count)
        .filter_map(|index| image::read_pointer(files.begin + index * 8))
        .filter_map(read_path)
        .collect()
}

/// The app's `operator new` and sized `operator delete`, resolved by name so
/// the vector the app frees is the one it would have allocated itself.
fn allocator() -> Option<(
    unsafe extern "C" fn(usize) -> *mut u8,
    unsafe extern "C" fn(*mut u8, usize),
)> {
    // SAFETY: both names are NUL-terminated and `dlsym` only looks them up.
    let new = unsafe { libc::dlsym(libc::RTLD_DEFAULT, c"_Znwm".as_ptr()) };
    // SAFETY: as above.
    let delete = unsafe { libc::dlsym(libc::RTLD_DEFAULT, c"_ZdlPvm".as_ptr()) };
    if new.is_null() || delete.is_null() {
        return None;
    }
    // SAFETY: these are libstdc++'s `operator new(size_t)` and
    // `operator delete(void *, size_t)`.
    unsafe {
        Some((
            core::mem::transmute::<*mut libc::c_void, unsafe extern "C" fn(usize) -> *mut u8>(new),
            core::mem::transmute::<*mut libc::c_void, unsafe extern "C" fn(*mut u8, usize)>(delete),
        ))
    }
}

/// Build a `std::vector<juce::File>` the app can own, from `paths`.
///
/// `None` when the app's allocator or string constructor is unavailable; the
/// caller then keeps the app's own listing.
///
/// # Safety
///
/// Runs the app's string constructor: call it on a thread the app builds
/// strings on (any; `juce::String` is thread-safe) after the image base is set.
#[must_use]
pub unsafe fn build_files(paths: &[PathBuf]) -> Option<RawFiles> {
    let base = image::base();
    if base == 0 {
        return None;
    }
    let (new, _) = allocator()?;
    if paths.is_empty() {
        return Some(RawFiles {
            begin: 0,
            end: 0,
            capacity: 0,
        });
    }
    let texts: Vec<CString> = paths
        .iter()
        .filter_map(|path| CString::new(path.as_os_str().as_bytes()).ok())
        .collect();
    let bytes = texts.len().checked_mul(8)?;
    // SAFETY: the app's own `operator new`; it throws instead of returning null.
    let begin = unsafe { new(bytes) } as usize;
    if begin == 0 {
        return None;
    }
    let make: unsafe extern "C" fn(usize, *const libc::c_char) =
        // SAFETY: `String(const char *)` of the verified firmware.
        unsafe { core::mem::transmute::<usize, _>(base + STRING_FROM_C) };
    for (index, text) in texts.iter().enumerate() {
        // SAFETY: each slot is 8 bytes of the new vector; the constructor
        // writes one string pointer there.
        unsafe { make(begin + index * 8, text.as_ptr()) };
    }
    Some(RawFiles {
        begin,
        end: begin + bytes,
        capacity: begin + bytes,
    })
}

/// Destroy a vector the app built: drop every string and free the storage.
///
/// # Safety
///
/// `files` must be a vector the app (or [`build_files`]) built, owned by the
/// caller and never used again.
pub unsafe fn release_files(files: RawFiles) {
    let base = image::base();
    let Some(count) = files.len() else {
        return;
    };
    let Some((_, delete)) = allocator() else {
        return;
    };
    let drop_string: unsafe extern "C" fn(usize) =
        // SAFETY: `String::~String` of the verified firmware.
        unsafe { core::mem::transmute::<usize, _>(base + STRING_DROP) };
    for index in 0..count {
        // SAFETY: every element is a live string the vector owns.
        unsafe { drop_string(files.begin + index * 8) };
    }
    if files.begin != 0 {
        // SAFETY: the storage came from `operator new` with this size.
        unsafe { delete(files.begin as *mut u8, files.capacity - files.begin) };
    }
}

/// Ask the browser to list its folder again, which calls
/// [`crate::Mod::on_browser_list`] once more.
///
/// Safe from any callback: the refresh is posted to the app's UI thread
/// ([`super::ui::post`]) and runs on its next turn.  Returns `false` when it
/// could not be posted.
#[must_use]
pub fn refresh() -> bool {
    super::ui::post(refresh_posted)
}

/// The posted half of [`refresh`].
fn refresh_posted() {
    // SAFETY: `ui::post` runs this on the UI thread.
    let _ = unsafe { refresh_now() };
}

/// Run the stock refresh right away.
///
/// Returns `false` before the browser has listed anything.
///
/// # Safety
///
/// Must run on the app's UI thread (a posted job, a paint, or
/// [`crate::Mod::on_browser_list`]'s thread): the list box updates from it.
#[must_use]
pub unsafe fn refresh_now() -> bool {
    let Some(contents) = contents() else {
        return false;
    };
    #[cfg(target_arch = "aarch64")]
    {
        // SAFETY: `REFRESH` is the 1.3.0.12 entry, verified by the loader's
        // firmware words, and `contents` was checked against its vtable.
        unsafe { image::void_of_pointer(image::base() + REFRESH, contents) };
        true
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        let _ = (contents, REFRESH);
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_folder_on_the_card_is_external_and_everything_else_internal() {
        assert_eq!(
            Location::of(Path::new("/media/MPC SAMPLE/MPC-Sample/Samples")),
            Location::External
        );
        assert_eq!(
            Location::of(Path::new("/data/UserContent/Samples/User")),
            Location::Internal
        );
        assert_eq!(
            Location::of(Path::new("/content/Samples/808")),
            Location::Internal
        );
        // A folder merely named like the card root is not on the card.
        assert_eq!(Location::of(Path::new("/data/media")), Location::Internal);
    }

    #[test]
    fn only_absolute_normalised_paths_are_listable() {
        assert!(is_listable(Path::new(
            "/data/UserContent/Samples/Kick 1.wav"
        )));
        assert!(is_listable(Path::new("/media/MPC SAMPLE/a/b.WAV")));
        assert!(!is_listable(Path::new("relative.wav")));
        assert!(!is_listable(Path::new("/")));
        assert!(!is_listable(Path::new("/data/folder/")));
        assert!(!is_listable(Path::new("/data//double.wav")));
        assert!(!is_listable(Path::new(OsStr::from_bytes(
            b"/data/nul\0.wav"
        ))));
    }

    #[test]
    fn the_root_folder_names_the_kind_of_listing() {
        assert_eq!(Kind::of_root(Path::new("/content/Samples")), Kind::Samples);
        assert_eq!(
            Kind::of_root(Path::new("/media/MPC SAMPLE/MPC-Sample/Samples")),
            Kind::Samples
        );
        assert_eq!(
            Kind::of_root(Path::new("/content/Projects")),
            Kind::Projects
        );
        assert_eq!(Kind::of_root(Path::new("/content/Kits")), Kind::Other);
        assert_eq!(Kind::of_root(Path::new("/")), Kind::Other);
    }

    #[test]
    fn a_cards_root_says_nothing_so_the_folder_does() {
        let card = Path::new("/media/MPC SAMPLE");
        let of = |folder: &str| Kind::of(card, Some(Path::new(folder)));
        assert_eq!(of("/media/MPC SAMPLE/MPC-Sample/Samples"), Kind::Samples);
        assert_eq!(
            of("/media/MPC SAMPLE/MPC-Sample/Samples/Drums/Kicks"),
            Kind::Samples
        );
        assert_eq!(of("/media/MPC SAMPLE/MPC-Sample/Projects"), Kind::Projects);
        assert_eq!(of("/media/MPC SAMPLE/Samples"), Kind::Samples);
        assert_eq!(of("/media/MPC SAMPLE/MPC-Sample"), Kind::Other);
        assert_eq!(of("/media/MPC SAMPLE"), Kind::Other);
        assert_eq!(of("/media/MPC SAMPLE/MPC-Sample/Other"), Kind::Other);
        assert_eq!(of("/media/Other/Samples"), Kind::Other);
        assert_eq!(Kind::of(card, None), Kind::Other);
        // A root that names its kind needs no folder.
        assert_eq!(Kind::of(Path::new("/content/Samples"), None), Kind::Samples);
        assert_eq!(
            Kind::of(
                Path::new("/content/Projects"),
                Some(Path::new("/content/Projects/Demo"))
            ),
            Kind::Projects
        );
    }

    #[test]
    fn only_the_visible_flag_makes_the_screen_visible() {
        assert!(visible(0x02));
        assert!(visible(0x03));
        assert!(!visible(0x00));
        assert!(!visible(0x01));
        assert!(!screen_visible());
        assert!(!is_open());
        assert_eq!(cursor_row(), None);
        assert_eq!(state(), None);
    }

    #[test]
    fn the_cursor_row_maps_to_what_that_contents_object_last_showed() {
        let mut shown = Shown::new();
        assert_eq!(shown.row(1, 0), None);
        shown.record(
            1,
            vec![PathBuf::from("/data/a.wav"), PathBuf::from("/data/b.wav")],
            false,
        );
        shown.record(2, vec![PathBuf::from("/media/x.wav")], true);
        assert_eq!(shown.row(1, 1), Some(PathBuf::from("/data/b.wav")));
        assert_eq!(shown.row(2, 0), Some(PathBuf::from("/media/x.wav")));
        assert_eq!(shown.row(1, 2), None);
        assert_eq!(shown.row(1, -1), None);
        // A new listing replaces the old one for the same contents object.
        shown.record(1, vec![PathBuf::from("/data/c.wav")], false);
        assert_eq!(shown.row(1, 0), Some(PathBuf::from("/data/c.wav")));
        assert_eq!(shown.row(1, 1), None);
        // The table never grows past the contents objects the app has.
        for contents in 10..30 {
            shown.record(contents, Vec::new(), false);
        }
        assert_eq!(shown.entries.len(), MAX_CONTENTS);
    }

    #[test]
    fn the_rows_on_screen_are_what_the_contents_object_last_showed() {
        let mut shown = Shown::new();
        assert_eq!(shown.files(1), Vec::<PathBuf>::new());
        let rows = vec![PathBuf::from("/data/a.wav"), PathBuf::from("/data/b.wav")];
        shown.record(1, rows.clone(), false);
        shown.record(2, vec![PathBuf::from("/media/x.wav")], true);
        assert_eq!(shown.files(1), rows);
        assert_eq!(shown.files(2), [PathBuf::from("/media/x.wav")]);
        shown.record(1, Vec::new(), true);
        assert_eq!(shown.files(1), Vec::<PathBuf>::new());
    }

    #[test]
    fn a_contents_object_shows_a_replacement_only_when_a_mod_made_it() {
        let mut shown = Shown::new();
        assert!(!shown.is_replaced(1), "nothing is known about it yet");
        shown.record(1, vec![PathBuf::from("/data/a.wav")], false);
        shown.record(2, vec![PathBuf::from("/media/x.wav")], true);
        assert!(!shown.is_replaced(1));
        assert!(shown.is_replaced(2));
        // The next listing decides again.
        shown.record(2, vec![PathBuf::from("/media/x.wav")], false);
        assert!(!shown.is_replaced(2));
    }

    #[test]
    fn nothing_scrolls_or_lists_before_the_app_builds_the_browser() {
        assert_eq!(scrolled(), None);
        assert_eq!(listed(), Vec::<PathBuf>::new());
        assert!(!is_replaced());
    }

    #[test]
    fn only_a_replaced_listing_answers_the_row_lookup() {
        let mut shown = Shown::new();
        let flat = vec![
            PathBuf::from("/data/Kicks/a.wav"),
            PathBuf::from("/media/MPC SAMPLE/Hats/b.wav"),
        ];
        // Unknown and stock listings are left to the app.
        assert_eq!(shown.replaced_row(1, 0), Row::Stock);
        shown.record(1, vec![PathBuf::from("/data/a.wav")], false);
        assert_eq!(shown.replaced_row(1, 0), Row::Stock);
        // A replaced listing answers every row, past its end with nothing.
        shown.record(1, flat.clone(), true);
        assert_eq!(shown.replaced_row(1, 1), Row::Path(flat[1].clone()));
        assert_eq!(shown.replaced_row(1, 2), Row::Empty);
        assert_eq!(shown.replaced_row(1, -1), Row::Empty);
        // Back to the stock listing: the app answers again.
        shown.record(1, vec![PathBuf::from("/data/a.wav")], false);
        assert_eq!(shown.replaced_row(1, 0), Row::Stock);
    }

    #[test]
    fn a_listing_keeps_the_stock_files_until_replaced() {
        let stock = vec![PathBuf::from("/data/a.wav"), PathBuf::from("/data/b.wav")];
        let mut listing = Listing::new(
            Some(Kind::Samples),
            Some(PathBuf::from("/data")),
            stock.clone(),
        );
        assert_eq!(listing.kind(), Some(Kind::Samples));
        assert_eq!(listing.files(), stock.as_slice());
        assert_eq!(listing.location(), Some(Location::Internal));
        assert!(listing.replacement().is_none());

        listing.replace(vec![
            PathBuf::from("/media/MPC SAMPLE/x.wav"),
            PathBuf::from("bad.wav"),
            PathBuf::from("/data/y.wav"),
        ]);
        assert_eq!(
            listing.replacement(),
            Some(
                [
                    PathBuf::from("/media/MPC SAMPLE/x.wav"),
                    PathBuf::from("/data/y.wav")
                ]
                .as_slice()
            )
        );
        // The stock files stay readable after a replacement.
        assert_eq!(listing.files(), stock.as_slice());
        assert_eq!(listing.take_replacement().map(|files| files.len()), Some(2));
        assert!(listing.replacement().is_none());
    }

    #[test]
    fn an_empty_replacement_is_still_a_replacement() {
        let mut listing = Listing::new(None, None, vec![PathBuf::from("/data/a.wav")]);
        listing.replace(Vec::new());
        assert_eq!(listing.replacement(), Some([].as_slice()));
        assert_eq!(listing.location(), None);
    }

    #[test]
    fn vector_pointers_are_checked_before_use() {
        assert_eq!(files_in(0, 0, 0), Some(0));
        assert_eq!(files_in(0x1000, 0x1010, 0x1018), Some(2));
        assert_eq!(files_in(0x1000, 0x1000, 0x1000), Some(0));
        assert_eq!(files_in(0, 8, 8), None);
        assert_eq!(files_in(0x1000, 0x0ff8, 0x1010), None);
        assert_eq!(files_in(0x1000, 0x1010, 0x1008), None);
        assert_eq!(files_in(0x1000, 0x1004, 0x1008), None);
        let empty = RawFiles {
            begin: 0,
            end: 0,
            capacity: 0,
        };
        assert!(empty.is_empty());
        assert_eq!(read_files(&empty), Vec::<PathBuf>::new());
    }

    #[test]
    fn string_bytes_become_absolute_paths_only() {
        assert_eq!(
            path_from_bytes(b"/data/UserContent/Samples"),
            Some(PathBuf::from("/data/UserContent/Samples"))
        );
        assert_eq!(path_from_bytes(b""), None);
        assert_eq!(path_from_bytes(b"Samples"), None);
        assert_eq!(read_path(0), None);
    }

    #[test]
    fn nothing_is_known_before_the_app_builds_its_browser() {
        assert_eq!(screen(), 0);
        assert_eq!(contents(), None);
        assert_eq!(folder(), None);
        assert_eq!(location(), None);
        // SAFETY: with no contents object nothing native is called.
        assert!(!unsafe { refresh_now() });
        assert!(!refresh());
    }

    #[test]
    fn the_offsets_are_the_documented_ones() {
        assert_eq!(BUILD_LIST, 0x14f_8d60);
        assert_eq!(GET_FILE, 0x14f_88f0);
        assert_eq!(REFRESH, 0x14f_95c0);
        assert_eq!(SCREEN_CTOR, 0x160_bbf0);
        assert_eq!(CONTENTS_VTABLE, 0x6d8_cb40);
        assert_eq!(SCREEN_VTABLE, 0x6e1_cfb8);
    }
}
