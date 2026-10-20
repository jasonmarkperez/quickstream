//! Embedded local Now Playing web server with live metadata, cover art, and remote control.
//!
//! - `state`: the snapshot the UI thread publishes (`NowPlayingState`) and the commands it receives (`WebCommand`).
//! - `server`: the listener, connection limits and request parsing.
//! - `api`: route dispatch, the REST API and the dashboard (`assets/index.html`).
//! - `stream`: byte-range file streaming and the continuous WAV stream.

mod api;
mod server;
mod state;
mod stream;

pub use server::{ServerLimits, WebBind, WebServer, WebServerHandle};
pub use state::{
    NowPlayingState, PlaylistEdit, PlaylistReply, WebBrowserEntry, WebBrowserResponse, WebCommand, WebDeviceItem,
    WebPlaylist, WebPlaylistEntry, WebPlaylists, WebPlaylistsState, WebQueueItem, WebReply, PLAYLISTS_CHANGED,
};
pub use stream::{
    check_stream_seek, make_continuous_wav_header, parse_byte_range, wav_header_sizes, ByteRange, ContinuousAdmission,
    ContinuousStreamController, StreamRequest, StreamServerState,
};
