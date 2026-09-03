// An RSS feed reader, as a NanoTorrent plugin.
//
// Copy this into your plugins folder, tick it in Preferences ▸ Plugins,
// approve what it asks for and press Ok. It then adds an RSS menu to the menu
// bar, and a cog on its row in Preferences for setting up the first feed.
//
// The window has two lists: your feeds above, and the items of whichever feed
// is selected below. Click a feed to switch to it; click an item to download it.
//
// It is here as much to show what a plugin CAN do as to be useful: it uses
// every subsystem the host offers - the network, its own storage, a window,
// a menu, and a timer - and nothing else. There is no file access and no way to
// run a program, so the worst this can do is what the permission list says.
//
// ---------------------------------------------------------------------------
//   network  fetch feeds, and download the .torrent files they point at
//   data     remember your feed list and which one you were looking at
//   ui       a window, a menu of its own, and a Configure cog
//   add      add the torrents you click
//
// `network` together with `add` is what lets `add_torrent_url` exist: a feed
// that lists .torrent files rather than magnet links is the ordinary case.
//! permissions: network, data, ui, add

// How many ticks between automatic refreshes. The host ticks once a minute.
let poll_every = 15;
let ticks = 0;

// Only handlers can see these. Rhai functions are pure: the scope is given to
// the function NanoTorrent calls, and not to anything that function calls in
// turn, so every helper below takes what it needs as an argument.
let feeds = [];       // every feed URL, in the order they were added
let selected = "";    // the feed the lower list is showing
let items = [];       // that feed's items

// ---------------------------------------------------------------------------
// The stored feed list
// ---------------------------------------------------------------------------
// Newline-separated rather than JSON: a URL cannot contain a newline, and this
// is a list of URLs. `data_get` gives () when nothing was ever stored.

fn feed_list() {
    let raw = data_get("feeds");
    if raw == () { return []; }
    let out = [];
    for line in raw.split("\n") {
        // Rhai's `trim` edits the string in place and returns (), so it is a
        // statement of its own rather than something to assign from. Same for
        // every other string method that changes its subject.
        let url = line;
        url.trim();
        if url != "" { out.push(url); }
    }
    return out;
}

fn save_feeds(list) {
    let joined = "";
    for url in list {
        if joined != "" { joined += "\n"; }
        joined += url;
    }
    // False means the store is full. Reported rather than ignored: a feed that
    // silently failed to save would come back missing after a restart.
    if !data_set("feeds", joined) {
        ui_status("Could not save the feed list - the plugin's storage is full.");
        return false;
    }
    return true;
}

// ---------------------------------------------------------------------------
// Reading a feed
// ---------------------------------------------------------------------------
// `parse_xml` hands back nested maps: #{ tag, attrs, text, children }. That is
// enough for both RSS (<item>) and Atom (<entry>) without knowing which one
// this is - collect either, then look for a link inside.

fn find_items(node) {
    let out = [];
    if node.tag == "item" || node.tag == "entry" { out.push(node); }
    for child in node.children {
        for found in find_items(child) { out.push(found); }
    }
    return out;
}

fn child_text(node, tag) {
    for child in node.children {
        if child.tag == tag { return child.text; }
    }
    return "";
}

// An <enclosure url> first: when a feed has both, that is the torrent and
// <link> is the web page describing it.
fn item_link(item) {
    for child in item.children {
        if child.tag == "enclosure" && "url" in child.attrs {
            return child.attrs.url;
        }
    }
    for child in item.children {
        if child.tag == "link" {
            // Atom puts the target in href; RSS puts it in the element text.
            if "href" in child.attrs && child.attrs.href != "" {
                return child.attrs.href;
            }
            if child.text != "" { return child.text; }
        }
    }
    return "";
}

// The host part of a URL, as a readable name for the feed list.
fn feed_name(url) {
    let s = url;
    s.replace("https://", "");
    s.replace("http://", "");
    let slash = s.index_of("/");
    if slash > 0 { s = s.sub_string(0, slash); }
    if s == "" { return url; }
    return s;
}

fn read_feed(url) {
    let out = [];
    let response = http_get(url);
    if !response.ok {
        let why = response.error;
        if why == "" { why = "HTTP " + response.status; }
        log("rss: " + url + " failed: " + why);
        ui_status("Could not read " + feed_name(url) + ": " + why);
        return out;
    }

    let doc = parse_xml(response.body);
    if doc == () {
        log("rss: " + url + " did not parse as XML");
        ui_status(feed_name(url) + " did not return XML.");
        return out;
    }

    for item in find_items(doc) {
        let link = item_link(item);
        if link == "" { continue; }
        let title = child_text(item, "title");
        if title == "" { title = link; }
        let when = child_text(item, "pubDate");
        if when == "" { when = child_text(item, "updated"); }
        out.push(#{ title: title, link: link, when: when });
    }
    return out;
}

// ---------------------------------------------------------------------------
// Drawing
// ---------------------------------------------------------------------------

fn draw_feeds(list, current) {
    let rows = [];
    for url in list {
        rows.push(#{
            id: url,
            title: feed_name(url),
            subtitle: url,
            selected: url == current,
        });
    }
    ui_groups(rows);
}

fn draw_items(list) {
    let rows = [];
    for item in list {
        rows.push(#{ id: item.link, title: item.title, subtitle: item.when });
    }
    ui_rows(rows);
}

// Read one feed and say how it went. Returns its items.
fn load_one(url) {
    if url == "" {
        ui_status("Add a feed URL above, then pick it from the list.");
        return [];
    }
    ui_status("Checking " + feed_name(url) + "...");
    let found = read_feed(url);
    if !found.is_empty() {
        ui_status(found.len() + " item(s) from " + feed_name(url) + ". Click one to download it.");
    }
    return found;
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

fn on_session_start() {
    ui_window("RSS feeds");
    ui_input("Feed URL");
    ui_buttons([
        #{ id: "add", label: "Add feed" },
        #{ id: "refresh", label: "Refresh" },
        #{ id: "forget", label: "Remove feed" },
    ]);

    // A dropdown of our own in the menu bar. One per plugin - calling this
    // again replaces the menu rather than adding a second.
    ui_menu("RSS", [
        #{ id: "open", label: "Feeds..." },
        #{ id: "refresh", label: "Check this feed now" },
    ]);

    // This plugin is useless until someone gives it a feed URL, which is
    // exactly what the Configure cog in Preferences is for.
    ui_configurable(true);

    feeds = feed_list();
    selected = data_get("selected");
    if selected == () { selected = ""; }
    if selected == "" && !feeds.is_empty() { selected = feeds[0]; }
    draw_feeds(feeds, selected);

    if feeds.is_empty() {
        ui_status("No feeds yet. Paste a feed URL above and press Add feed.");
    } else {
        ui_status("Open the window from the RSS menu.");
    }
    // Deliberately not fetching here: a plugin should not reach the network
    // because the application started.
}

// A feed in the upper list was clicked: show that one.
fn on_ui_group(id) {
    selected = id;
    data_set("selected", selected);
    draw_feeds(feeds, selected);
    items = load_one(selected);
    draw_items(items);
}

// The window was opened. Show what is already known rather than an empty list,
// and only fetch if nothing has been read yet.
fn on_ui_open() {
    feeds = feed_list();
    if selected == "" && !feeds.is_empty() { selected = feeds[0]; }
    draw_feeds(feeds, selected);
    draw_items(items);
    if items.is_empty() && selected != "" {
        items = load_one(selected);
        draw_items(items);
    }
}

fn on_ui_button(id, input) {
    let url = input;
    url.trim();

    if id == "add" {
        if url == "" {
            ui_status("Type a feed URL first.");
            return;
        }
        feeds = feed_list();
        if url in feeds {
            ui_status("That feed is already in the list.");
            return;
        }
        feeds.push(url);
        if !save_feeds(feeds) { return; }
        // Switch to what was just added: that is what someone adding a feed
        // wants to look at.
        selected = url;
        data_set("selected", selected);
    } else if id == "forget" {
        let drop = url;
        if drop == "" { drop = selected; }
        let kept = [];
        for existing in feed_list() {
            if existing != drop { kept.push(existing); }
        }
        if !save_feeds(kept) { return; }
        feeds = kept;
        if selected == drop {
            selected = "";
            if !feeds.is_empty() { selected = feeds[0]; }
            data_set("selected", selected);
            items = [];
        }
    } else if id != "refresh" {
        return;
    }

    draw_feeds(feeds, selected);
    items = load_one(selected);
    draw_items(items);
}

// An item was clicked: its id is the link the feed gave.
fn on_ui_row(id) {
    if add_torrent_url(id) {
        ui_status("Added: " + id);
    } else {
        ui_status("Could not add: " + id);
    }
}

fn on_ui_menu(id) {
    if id == "open" {
        ui_show();
    } else if id == "refresh" {
        ui_show();
        items = load_one(selected);
        draw_items(items);
    }
}

// Configure, on this plugin's row in Preferences. Its window IS its settings -
// the feed list is the whole of what there is to configure.
fn on_ui_configure() {
    ui_show();
}

fn on_tick() {
    ticks += 1;
    if ticks >= poll_every {
        ticks = 0;
        if selected != "" {
            items = load_one(selected);
            draw_items(items);
        }
    }
}
