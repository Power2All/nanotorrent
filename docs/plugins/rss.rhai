// An RSS reader with an auto-downloader, as a NanoTorrent plugin.
//
// Copy this into your plugins folder, tick it in Preferences ▸ Plugins,
// approve what it asks for and press Ok. It then adds an RSS menu to the menu
// bar, and a cog on its row in Preferences.
//
// It is a full RSS reader in the shape a plugin can be: a list of feeds,
// checked on a timer, and a list of RULES that download matching items on
// their own. Nothing here is built into NanoTorrent - every line runs on the
// same plugin API any script can use.
//
// ---------------------------------------------------------------------------
// USING IT
//
// The upper list holds your feeds plus one entry called "Rules". Click a feed
// to read it; click an article to download it. Click "Rules" to manage the
// auto-downloader, and a rule to open its editor.
//
// "Settings" opens the four that belong to the reader as a whole: how often to
// check, how many articles to keep per feed, whether the auto-downloader runs
// at all, and whether a REPACK or PROPER re-release is allowed to replace an
// episode already taken.
//
// ---------------------------------------------------------------------------
// RULES
//
// A rule is a form, not a line of syntax. Its fields:
//
//   Must contain / must not contain
//       Every whitespace-separated term has to appear somewhere in the title,
//       in any order, ignoring case. A term may offer alternatives with "|":
//
//           ubuntu|debian|fedora amd64
//
//       means "(ubuntu OR debian OR fedora) AND amd64". "Must not contain"
//       works the same way and rejects the item if anything matches.
//
//       A comma does the same as "|". That is what this plugin used before the
//       rule form existed, when "|" was the field separator in the single text
//       box, and rules written then still work.
//
//   Use regular expressions
//       Treats both fields as regular expressions instead. Patterns run
//       against the title as it appears; put "(?i)" at the front for a
//       case-insensitive match. The engine has no backtracking, so a pattern
//       from a feed you do not control cannot hang the client.
//
//       A field beginning "re:" is treated as a pattern whether or not the box
//       is ticked. That is how this plugin spelled it before the box existed,
//       and rules written then still work.
//
//   Episode filter
//       The conventional season/episode syntax:
//
//           1x25-;      season 1 episode 25, and everything after it
//           1x1-10;     season 1, episodes 1 to 10
//           2x;         all of season 2
//           1x2;1x5;    two single episodes
//
//       The season and episode are read from the title in either of the two
//       forms anybody writes them: "S01E02" or "1x02".
//
//   Smart episode filter
//       Take each episode once. The rule remembers what it has already
//       matched, so a re-upload of the same episode is skipped - which is what
//       this exists for. A REPACK or PROPER is let through anyway when the
//       setting of that name is on.
//
//   Ignore subsequent matches for (days)
//       After a match, the rule sits out this many days. 0 turns it off.
//
//   Label / save path / add paused
//       Applied to whatever the rule adds. The label must already exist -
//       a rule naming one that does not gets no label rather than creating it.
//
//   Apply to feeds
//       Blank means every feed. Otherwise a comma-separated list of feed URLs,
//       and the rule only looks at those.
//
// ---------------------------------------------------------------------------
// WHAT IS NOT HERE, AND WHY
//
// No folders in the feed list. The window offers one list of feeds and one of
// articles, and a tree would be a third thing to draw with two lists.
//
// No "import/export rules". The rules live in the plugin's own store, which
// travels with the settings database - and Preferences ▸ Database already
// exports that.
//
// ---------------------------------------------------------------------------
//   network  fetch feeds, and download the .torrent files they point at
//   data     remember feeds, rules, settings, and what has already been taken
//   ui       a window, a menu of its own, and a Configure cog
//   add      add the torrents that match a rule, and the ones you click
//! permissions: network, data, ui, add

let ticks = 0;

// Only handlers can see these. Rhai functions are pure: the scope is given to
// the function NanoTorrent calls, and not to anything that function calls in
// turn, so every helper below takes what it needs as an argument.
let feeds = [];       // every feed URL, in the order they were added
let selected = "";    // the feed the lower list is showing, or RULES
let items = [];       // that feed's items
let rules = [];       // the auto-download rules
let editing = "";     // the rule the open form belongs to, "" for a new one

// The id of the pseudo-feed that shows the rules instead of articles. A URL
// can never look like this, so it cannot collide with a real feed.
const RULES = "\u0001rules";

// At most this many links are remembered as already-downloaded. Old entries
// fall off the end: a feed only lists recent items, so an item old enough to
// have been pushed out is one the feed has long stopped mentioning.
const SEEN_MAX = 500;

// ---------------------------------------------------------------------------
// Stored lists
// ---------------------------------------------------------------------------
// Newline-separated, because `data_set` stores strings and the host offers no
// list type. Blank lines are dropped on the way out, so a trailing newline is
// not an empty entry.

fn lines_of(key) {
    let raw = data_get(key);
    if raw == () { return []; }
    let out = [];
    for line in raw.split("\n") {
        let one = line;
        one.trim();
        if one != "" { out.push(one); }
    }
    return out;
}

fn join_lines(list) {
    let text = "";
    for line in list {
        if text != "" { text += "\n"; }
        text += line;
    }
    return text;
}

fn store(key, list) {
    if !data_set(key, join_lines(list)) {
        ui_status("The plugin store is full - remove a feed or a rule.");
        return false;
    }
    return true;
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------
// Four values, each with a default, kept under their own keys so adding a
// fifth needs no migration.

// Rhai's `parse_int` throws on anything that is not a number, and these
// numbers come out of a form somebody typed into. Checked first, so a stray
// letter falls back to the default instead of aborting the handler.
//
// Character by character rather than with a regex: this is called on every
// draw, and a helper this small should not drag the regex engine in behind it.
fn to_int(text, fallback) {
    let s = "" + text;
    s.trim();
    if s == "" { return fallback; }
    let start = 0;
    if s[0] == '-' { start = 1; }
    if s.len() <= start { return fallback; }
    for i in start..s.len() {
        let c = s[i];
        if c < '0' || c > '9' { return fallback; }
    }
    return parse_int(s);
}

fn setting(key, fallback) {
    let raw = data_get("set." + key);
    if raw == () || raw == "" { return fallback; }
    return raw;
}

fn setting_int(key, fallback) {
    return to_int(setting(key, ""), fallback);
}

fn setting_on(key, fallback) {
    let raw = data_get("set." + key);
    if raw == () { return fallback; }
    return raw != "";
}

// ---------------------------------------------------------------------------
// Rules
// ---------------------------------------------------------------------------
// One per line, tab-separated, in this order:
//
//   enabled name must must_not save_path regex episodes smart ignore_days
//   label paused feeds last_match
//
// Positional and tolerant of a short line, which is what makes it backward
// compatible: a rule written by the five-field version still loads, and comes
// back with the new fields at their defaults.

fn rule_field(parts, index, fallback) {
    if parts.len() > index { return parts[index]; }
    return fallback;
}

fn parse_rule(line) {
    let parts = line.split("\t");
    if parts.len() < 3 { return (); }
    return #{
        enabled: parts[0] == "1",
        name: parts[1],
        must: parts[2],
        must_not: rule_field(parts, 3, ""),
        save_path: rule_field(parts, 4, ""),
        regex: rule_field(parts, 5, "") == "1",
        episodes: rule_field(parts, 6, ""),
        smart: rule_field(parts, 7, "") == "1",
        ignore_days: to_int(rule_field(parts, 8, "0"), 0),
        label: rule_field(parts, 9, ""),
        paused: rule_field(parts, 10, "") == "1",
        feeds: rule_field(parts, 11, ""),
        last_match: to_int(rule_field(parts, 12, "0"), 0),
    };
}

// A tab or a newline in a field would split the record, so they go. Trimming
// rather than refusing: a pasted save path with a stray tab is a typo, not an
// attack, and losing the rule would be the worse answer.
fn clean(text) {
    let s = text;
    s.replace("\t", " ");
    s.replace("\n", " ");
    s.replace("\r", " ");
    s.trim();
    return s;
}

// "1" or "0", for the stored rule line - a positional record wants a
// character in every field, so an empty one would shift the rest along.
fn flag(on) {
    if on { return "1"; }
    return "0";
}

// "1" or "", for anything crossing to the host or into a form: a checkbox
// arrives that way, and the host reads any non-empty value as true.
fn on_off(on) {
    if on { return "1"; }
    return "";
}

fn number_text(n) {
    if n == () { return "0"; }
    return "" + n;
}

fn rule_line(rule) {
    return flag(rule.enabled) + "\t" + clean(rule.name) + "\t" + clean(rule.must)
        + "\t" + clean(rule.must_not) + "\t" + clean(rule.save_path)
        + "\t" + flag(rule.regex) + "\t" + clean(rule.episodes)
        + "\t" + flag(rule.smart) + "\t" + number_text(rule.ignore_days)
        + "\t" + clean(rule.label) + "\t" + flag(rule.paused)
        + "\t" + clean(rule.feeds) + "\t" + number_text(rule.last_match);
}

fn rule_list() {
    let out = [];
    for line in lines_of("rules") {
        let rule = parse_rule(line);
        if rule != () { out.push(rule); }
    }
    return out;
}

fn save_rules(list) {
    let lines = [];
    for rule in list { lines.push(rule_line(rule)); }
    return store("rules", lines);
}

fn find_rule(list, name) {
    for rule in list {
        if rule.name == name { return rule; }
    }
    return ();
}

fn blank_rule() {
    return #{
        enabled: true, name: "", must: "", must_not: "", save_path: "",
        regex: false, episodes: "", smart: false, ignore_days: 0,
        label: "", paused: false, feeds: "", last_match: 0,
    };
}

// ---------------------------------------------------------------------------
// Matching
// ---------------------------------------------------------------------------
// The word form: every whitespace-separated term must appear, and a term may
// offer alternatives separated by "|". Case is ignored throughout.

fn term_matches(title_lower, term) {
    // "|" is what the rule form documents. "," is what this plugin used before
    // the form existed, when "|" was already spoken for as the field separator
    // in the one text box there was. Both are accepted, so a rule written
    // either way keeps working.
    let alternatives = term;
    alternatives.replace(",", "|");
    for option in alternatives.split("|") {
        let needle = option;
        needle.trim();
        needle.make_lower();
        if needle == "" { continue; }
        if title_lower.contains(needle) { return true; }
    }
    return false;
}

fn all_terms_match(title_lower, expression) {
    for term in expression.split(" ") {
        let one = term;
        one.trim();
        if one == "" { continue; }
        if !term_matches(title_lower, one) { return false; }
    }
    return true;
}

fn any_term_matches(title_lower, expression) {
    for term in expression.split(" ") {
        let one = term;
        one.trim();
        if one == "" { continue; }
        if term_matches(title_lower, one) { return true; }
    }
    return false;
}

// A field is either a word expression or a regular expression, decided by the
// rule's own "use regular expressions" box. The host provides `regex_match` -
// its engine has no backtracking, which is why a pattern out of a feed nobody
// vetted is safe to run.
//
// The regex is matched against the ORIGINAL title, not the lowercased copy, so
// character classes and anchors mean what they look like. Use the inline flag
// `(?i)` for a case-insensitive pattern; the word form ignores case already.
fn field_matches(title, lower, expression, use_regex, need_all) {
    // "re:" in front of the field is how this plugin spelled a pattern before
    // the checkbox existed. Still honoured, per field, so a rule written then
    // keeps working - a rule that silently stops matching is the worst way to
    // find out about an upgrade.
    let pattern_here = expression.starts_with("re:");
    if use_regex || pattern_here {
        let pattern = expression;
        if pattern_here { pattern = expression.sub_string(3); }
        pattern.trim();
        if pattern == "" { return false; }
        return regex_match(pattern, title);
    }
    if need_all { return all_terms_match(lower, expression); }
    return any_term_matches(lower, expression);
}

// ---------------------------------------------------------------------------
// Episodes
// ---------------------------------------------------------------------------

// The season and episode a title names, or () when it names neither.
//
// Two forms, because those are the two anybody writes: "S01E02" (with or
// without a separator) and "1x02".
fn episode_of(title) {
    let found = regex_captures("(?i)s(\\d{1,3})[ ._-]?e(\\d{1,4})", title);
    if found.is_empty() {
        found = regex_captures("(?i)(?:^|[^\\d])(\\d{1,3})x(\\d{1,4})", title);
    }
    if found.len() < 3 { return (); }
    let season = to_int(found[1], -1);
    let episode = to_int(found[2], -1);
    if season < 0 || episode < 0 { return (); }
    return #{ season: season, episode: episode };
}

// One clause of an episode filter: "1x25-", "1x1-10", "2x", "1x2".
//
// `to` of -1 is open-ended, and carries on into later seasons - "episode 25
// and everything after it" means the rest of the show, not the rest of the
// season.
fn parse_episode_clause(text) {
    let part = text;
    part.trim();
    if part == "" { return (); }
    let at = part.index_of("x");
    if at < 1 { return (); }

    let season = to_int(part.sub_string(0, at), -1);
    if season < 0 { return (); }
    let spec = part.sub_string(at + 1);
    spec.trim();

    // "2x" - the whole season.
    if spec == "" { return #{ season: season, from: 1, to: -1, whole: true }; }

    let dash = spec.index_of("-");
    if dash < 0 {
        let one = to_int(spec, -1);
        if one < 0 { return (); }
        return #{ season: season, from: one, to: one, whole: false };
    }

    let from = to_int(spec.sub_string(0, dash), -1);
    if from < 0 { return (); }
    let tail = spec.sub_string(dash + 1);
    tail.trim();
    if tail == "" {
        return #{ season: season, from: from, to: -1, whole: false };
    }
    let to = to_int(tail, -1);
    if to < 0 { return (); }
    return #{ season: season, from: from, to: to, whole: false };
}

fn episode_filter_matches(filter, ep) {
    let any = false;
    for clause_text in filter.split(";") {
        let clause = parse_episode_clause(clause_text);
        if clause == () { continue; }
        any = true;

        if clause.whole {
            if ep.season == clause.season { return true; }
            continue;
        }
        if clause.to < 0 {
            // Open-ended: this episode onward, and every later season.
            if ep.season > clause.season { return true; }
            if ep.season == clause.season && ep.episode >= clause.from { return true; }
            continue;
        }
        if ep.season == clause.season && ep.episode >= clause.from && ep.episode <= clause.to {
            return true;
        }
    }
    // A filter that parsed to nothing at all is not a filter, so it cannot
    // reject anything - otherwise a typo would silently stop a rule.
    if !any { return true; }
    return false;
}

// Does this rule apply to this feed? Blank means every feed.
fn rule_covers_feed(rule, url) {
    let list = rule.feeds;
    list.trim();
    if list == "" { return true; }
    for one in list.split(",") {
        let want = one;
        want.trim();
        if want != "" && want == url { return true; }
    }
    return false;
}

fn is_repack(title) {
    let lower = title;
    lower.make_lower();
    return lower.contains("repack") || lower.contains("proper");
}

// Everything about the title itself. The clock and the episode history are
// checked separately, in `run_rules`, because they are about the rule's
// history rather than about this item.
fn rule_matches(rule, title) {
    if !rule.enabled { return false; }
    let lower = title;
    lower.make_lower();

    if rule.must != "" && !field_matches(title, lower, rule.must, rule.regex, true) {
        return false;
    }
    // A rule with no "must contain" and no episode filter would match every
    // item in every feed, which is never what anybody meant.
    if rule.must == "" && rule.episodes == "" { return false; }

    if rule.must_not != "" && field_matches(title, lower, rule.must_not, rule.regex, false) {
        return false;
    }

    if rule.episodes != "" {
        let ep = episode_of(title);
        // A rule with an episode filter is about a series; an item whose title
        // names no episode is not one of its episodes.
        if ep == () { return false; }
        if !episode_filter_matches(rule.episodes, ep) { return false; }
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

fn read_feed(url, quiet, limit) {
    let out = [];
    let response = http_get(url);
    if !response.ok {
        let why = response.error;
        if why == "" { why = "HTTP " + response.status; }
        log("rss: " + url + " failed: " + why);
        // A background sweep says nothing: a feed that is down for an hour
        // should not overwrite the status line every fifteen minutes.
        if !quiet { ui_status("Could not read " + feed_name(url) + ": " + why); }
        return out;
    }

    let doc = parse_xml(response.body);
    if doc == () {
        log("rss: " + url + " did not parse as XML");
        if !quiet { ui_status(feed_name(url) + " did not return XML."); }
        return out;
    }

    for item in find_items(doc) {
        // Feeds are newest-first, so stopping at the limit keeps the newest.
        if limit > 0 && out.len() >= limit { break; }
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
// The auto-downloader
// ---------------------------------------------------------------------------

// Episodes a rule has already taken, so the smart filter can skip a re-upload.
fn episode_key(ep) {
    return "" + ep.season + "x" + ep.episode;
}

// Read every feed, download whatever matches a rule, and report the count.
//
// The "seen" list is written back once at the end rather than per item: it is
// the only thing standing between a restart and downloading the whole feed
// again, but writing it five times in one sweep buys nothing.
fn run_rules(all_feeds, all_rules, limit, repacks) {
    let added = 0;
    if all_rules.is_empty() { return added; }

    let seen = lines_of("seen");
    let changed = false;
    let rules_changed = false;
    let right_now = now();

    for url in all_feeds {
        for item in read_feed(url, true, limit) {
            if item.link in seen { continue; }

            // By index, not `for rule in all_rules`: Rhai hands a `for` loop
            // a COPY of each element, so writing `last_match` back through one
            // would update nothing and the ignore window would never start.
            for index in 0..all_rules.len() {
                let rule = all_rules[index];
                if !rule_covers_feed(rule, url) { continue; }
                if !rule_matches(rule, item.title) { continue; }

                // "Ignore subsequent matches for N days" - the rule sits out
                // the rest of the window after a hit.
                if rule.ignore_days > 0 && rule.last_match > 0 {
                    if right_now - rule.last_match < rule.ignore_days * 86400 {
                        continue;
                    }
                }

                // The smart filter: one copy of each episode, unless this is a
                // REPACK or PROPER and those are allowed to replace it.
                let ep = ();
                let key = "";
                if rule.smart {
                    ep = episode_of(item.title);
                    if ep != () {
                        key = episode_key(ep);
                        let taken = lines_of("ep." + rule.name);
                        if key in taken && !(repacks && is_repack(item.title)) {
                            continue;
                        }
                    }
                }

                let ok = add_torrent_url(item.link, #{
                    save_path: rule.save_path,
                    label: rule.label,
                    // "" for off, not "0": the host reads any non-empty value
                    // as true, which is the same rule a form checkbox follows.
                    paused: on_off(rule.paused),
                });

                if ok {
                    // Remembered only on success, so a feed that was briefly
                    // unreachable gets another go rather than being written off.
                    seen.push(item.link);
                    changed = true;
                    added += 1;
                    all_rules[index].last_match = right_now;
                    rules_changed = true;
                    if key != "" {
                        let taken = lines_of("ep." + rule.name);
                        if !(key in taken) {
                            taken.push(key);
                            while taken.len() > global::SEEN_MAX { taken.shift(); }
                            data_set("ep." + rule.name, join_lines(taken));
                        }
                    }
                    log("rss: " + rule.name + " matched " + item.title);
                    notify("RSS: " + rule.name, item.title);
                } else {
                    log("rss: could not add " + item.link);
                }
                // One rule per item. The first match wins, which is what makes
                // rule order meaningful.
                break;
            }
        }
    }

    if changed {
        // Trim from the front: the oldest links are the ones the feeds have
        // stopped listing.
        // `global::` because a script-level constant is not in scope inside a
        // function - Rhai functions are pure, and see only what they are given.
        while seen.len() > global::SEEN_MAX { seen.shift(); }
        store("seen", seen);
    }
    // `last_match` moved, and it is what the ignore window is measured from.
    if rules_changed { save_rules(all_rules); }
    return added;
}

// ---------------------------------------------------------------------------
// Drawing
// ---------------------------------------------------------------------------

fn draw_feeds(list, current, rule_count) {
    let rows = [];
    for url in list {
        rows.push(#{
            id: url,
            title: feed_name(url),
            subtitle: url,
            selected: url == current,
        });
    }
    // The rules live at the bottom of the same list, because the window has one
    // upper list and this is the only place to put them.
    rows.push(#{
        id: global::RULES,
        title: "Rules",
        subtitle: rule_count + " auto-download rule(s)",
        selected: current == global::RULES,
    });
    ui_groups(rows);
}

fn draw_items(list) {
    let rows = [];
    for item in list {
        rows.push(#{ id: item.link, title: item.title, subtitle: item.when });
    }
    ui_rows(rows);
}

fn rule_summary(rule) {
    let detail = "off";
    if rule.enabled { detail = "on"; }
    if rule.regex { detail += " - regex"; }
    if rule.must != "" { detail += " - must: " + rule.must; }
    if rule.must_not != "" { detail += " / not: " + rule.must_not; }
    if rule.episodes != "" { detail += " - episodes: " + rule.episodes; }
    if rule.smart { detail += " - smart"; }
    if rule.ignore_days > 0 { detail += " - ignore " + rule.ignore_days + "d"; }
    if rule.label != "" { detail += " - label: " + rule.label; }
    if rule.save_path != "" { detail += " -> " + rule.save_path; }
    if rule.paused { detail += " (paused)"; }
    return detail;
}

fn draw_rules(list) {
    let rows = [];
    for rule in list {
        rows.push(#{ id: rule.name, title: rule.name, subtitle: rule_summary(rule) });
    }
    ui_rows(rows);
}

// The buttons depend on what is selected: a feed and a rule are not acted on
// in the same ways, and offering both sets at once means half of them do
// nothing wherever you are.
fn draw_buttons(current) {
    if current == global::RULES {
        ui_input("");
        ui_buttons([
            #{ id: "newrule", label: "New rule" },
            #{ id: "runrules", label: "Run rules now" },
            #{ id: "settings", label: "Settings" },
        ]);
        return;
    }
    ui_input("Feed URL");
    ui_buttons([
        #{ id: "add", label: "Add feed" },
        #{ id: "refresh", label: "Refresh" },
        #{ id: "forget", label: "Remove feed" },
        #{ id: "settings", label: "Settings" },
    ]);
}

// Read one feed and say how it went. Returns its items.
fn load_one(url, limit) {
    if url == "" {
        ui_status("Add a feed URL above, then pick it from the list.");
        return [];
    }
    ui_status("Checking " + feed_name(url) + "...");
    let found = read_feed(url, false, limit);
    if !found.is_empty() {
        ui_status(found.len() + " item(s) from " + feed_name(url) + ". Click one to download it.");
    }
    return found;
}

// ---------------------------------------------------------------------------
// Forms
// ---------------------------------------------------------------------------

fn show_settings_form() {
    ui_form("settings", "RSS settings", [
        #{
            id: "interval", label: "Check every (minutes)", kind: "number",
            value: "" + setting_int("interval", 15),
            hint: "How often every feed is read and the rules are run.",
        },
        #{
            id: "articles", label: "Keep per feed (articles)", kind: "number",
            value: "" + setting_int("articles", 50),
            hint: "Newest first. 0 keeps everything the feed offers.",
        },
        #{
            id: "auto", label: "Run the auto-downloader", kind: "check",
            value: on_off(setting_on("auto", true)),
            hint: "Off leaves the feeds readable and downloads nothing on its own.",
        },
        #{
            id: "repacks", label: "Take REPACK and PROPER re-releases", kind: "check",
            value: on_off(setting_on("repacks", false)),
            hint: "Only affects rules with the smart episode filter on.",
        },
    ]);
}

fn show_rule_form(rule) {
    let title = "New rule";
    if rule.name != "" { title = "Rule: " + rule.name; }
    ui_form("rule", title, [
        #{ id: "name", label: "Name", kind: "text", value: rule.name },
        #{ id: "enabled", label: "Enabled", kind: "check", value: on_off(rule.enabled) },
        #{
            id: "must", label: "Must contain", kind: "text", value: rule.must,
            hint: "Every term has to appear. Alternatives with |, as in ubuntu|debian.",
        },
        #{ id: "must_not", label: "Must not contain", kind: "text", value: rule.must_not },
        #{
            id: "regex", label: "Use regular expressions", kind: "check",
            value: on_off(rule.regex),
            hint: "Treats both fields above as patterns. (?i) at the front ignores case.",
        },
        #{
            id: "episodes", label: "Episode filter", kind: "text", value: rule.episodes,
            hint: "1x25- is episode 25 onward, 1x1-10 a range, 2x a whole season.",
        },
        #{
            id: "smart", label: "Smart episode filter", kind: "check",
            value: on_off(rule.smart),
            hint: "Take each episode once, however many times it is re-uploaded.",
        },
        #{
            id: "ignore_days", label: "Ignore later matches for (days)", kind: "number",
            value: "" + rule.ignore_days,
            hint: "0 is off.",
        },
        #{
            id: "label", label: "Label", kind: "text", value: rule.label,
            hint: "Must already exist - this will not create one.",
        },
        #{ id: "save_path", label: "Save to", kind: "text", value: rule.save_path },
        #{ id: "paused", label: "Add paused", kind: "check", value: on_off(rule.paused) },
        #{
            id: "feeds", label: "Apply to feeds", kind: "text", value: rule.feeds,
            hint: "Blank means every feed. Otherwise a comma-separated list of feed URLs.",
        },
    ]);
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

fn on_session_start() {
    ui_window("RSS feeds");

    // A dropdown of our own in the menu bar. One per plugin - calling this
    // again replaces the menu rather than adding a second.
    ui_menu("RSS", [
        #{ id: "open", label: "Feeds..." },
        #{ id: "refresh", label: "Check this feed now" },
        #{ id: "runrules", label: "Check every feed now" },
        #{ id: "settings", label: "Settings..." },
    ]);

    // This plugin is useless until someone gives it a feed URL, which is
    // exactly what the Configure cog in Preferences is for.
    ui_configurable(true);

    feeds = lines_of("feeds");
    rules = rule_list();
    selected = data_get("selected");
    if selected == () { selected = ""; }
    if selected == "" && !feeds.is_empty() { selected = feeds[0]; }
    draw_feeds(feeds, selected, rules.len());
    draw_buttons(selected);

    if feeds.is_empty() {
        ui_status("No feeds yet. Paste a feed URL above and press Add feed.");
    } else {
        ui_status("Open the window from the RSS menu.");
    }
    // Deliberately not fetching here: a plugin should not reach the network
    // because the application started.
}

// A row in the upper list was clicked: show that feed, or the rules.
fn on_ui_group(id) {
    selected = id;
    data_set("selected", selected);
    editing = "";
    ui_form_close();
    draw_feeds(feeds, selected, rules.len());
    draw_buttons(selected);

    if selected == global::RULES {
        rules = rule_list();
        draw_rules(rules);
        ui_status(rules.len() + " rule(s). Click one to edit it.");
    } else {
        items = load_one(selected, setting_int("articles", 50));
        draw_items(items);
    }
}

// The window was opened. Show what is already known rather than an empty list,
// and only fetch if nothing has been read yet.
fn on_ui_open() {
    feeds = lines_of("feeds");
    rules = rule_list();
    if selected == "" && !feeds.is_empty() { selected = feeds[0]; }
    draw_feeds(feeds, selected, rules.len());
    draw_buttons(selected);

    if selected == global::RULES {
        draw_rules(rules);
        return;
    }
    draw_items(items);
    if items.is_empty() && selected != "" {
        items = load_one(selected, setting_int("articles", 50));
        draw_items(items);
    }
}

fn on_ui_button(id, input) {
    let text = input;
    text.trim();

    if id == "settings" {
        show_settings_form();
        return;
    }

    if id == "newrule" {
        editing = "";
        show_rule_form(blank_rule());
        return;
    }

    if id == "runrules" {
        feeds = lines_of("feeds");
        rules = rule_list();
        ui_status("Checking " + feeds.len() + " feed(s) against " + rules.len() + " rule(s)...");
        let added = run_rules(feeds, rules, setting_int("articles", 50),
                              setting_on("repacks", false));
        rules = rule_list();
        draw_rules(rules);
        ui_status("Auto-download added " + added + " torrent(s).");
        return;
    }

    if id == "add" {
        if text == "" {
            ui_status("Type a feed URL first.");
            return;
        }
        feeds = lines_of("feeds");
        if text in feeds {
            ui_status("That feed is already in the list.");
            return;
        }
        feeds.push(text);
        if !store("feeds", feeds) { return; }
        // Switch to what was just added: that is what someone adding a feed
        // wants to look at.
        selected = text;
        data_set("selected", selected);
    } else if id == "forget" {
        let drop = text;
        if drop == "" { drop = selected; }
        let kept = [];
        for existing in lines_of("feeds") {
            if existing != drop { kept.push(existing); }
        }
        if !store("feeds", kept) { return; }
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

    draw_feeds(feeds, selected, rules.len());
    draw_buttons(selected);
    if selected == global::RULES {
        draw_rules(rules);
        return;
    }
    items = load_one(selected, setting_int("articles", 50));
    draw_items(items);
}

// A row in the lower list was clicked. In the rules view that opens the rule's
// editor; otherwise it is an article, and its id is the link the feed gave.
fn on_ui_row(id) {
    if selected == global::RULES {
        rules = rule_list();
        let rule = find_rule(rules, id);
        if rule == () {
            ui_status("That rule is gone - the list has been reloaded.");
            draw_rules(rules);
            return;
        }
        editing = id;
        show_rule_form(rule);
        return;
    }

    if add_torrent_url(id) {
        ui_status("Added: " + id);
    } else {
        ui_status("Could not add: " + id);
    }
}

// A form came back. `values` is field id to value, every one a string - a
// checkbox arrives as "1" or "".
fn on_ui_form(form_id, values) {
    if form_id == "settings" {
        data_set("set.interval", values.interval);
        data_set("set.articles", values.articles);
        data_set("set.auto", values.auto);
        data_set("set.repacks", values.repacks);
        ui_form_close();
        draw_buttons(selected);
        ui_status("Settings saved. Checking every " + setting_int("interval", 15) + " minute(s).");
        return;
    }

    if form_id != "rule" { return; }

    let name = values.name;
    name.trim();
    if name == "" {
        ui_status("A rule needs a name.");
        return;
    }
    let rule = #{
        enabled: values.enabled != "",
        name: name,
        must: values.must,
        must_not: values.must_not,
        save_path: values.save_path,
        regex: values.regex != "",
        episodes: values.episodes,
        smart: values.smart != "",
        ignore_days: to_int(values.ignore_days, 0),
        label: values.label,
        paused: values.paused != "",
        feeds: values.feeds,
        last_match: 0,
    };
    if rule.must == "" && rule.episodes == "" {
        ui_status("A rule needs something to match: fill in Must contain, or an episode filter.");
        return;
    }

    rules = rule_list();
    // Renaming keeps the rule's history: the old name is what the episode list
    // and the ignore window were stored under.
    let previous = find_rule(rules, editing);
    if previous != () { rule.last_match = previous.last_match; }

    let kept = [];
    for existing in rules {
        // The row being edited goes, and so does anything already using the
        // new name - a rename onto an existing rule replaces it rather than
        // leaving two rules that cannot be told apart in the list.
        if existing.name != editing && existing.name != rule.name {
            kept.push(existing);
        }
    }
    kept.push(rule);
    if !save_rules(kept) { return; }

    if editing != "" && editing != rule.name {
        let history = data_get("ep." + editing);
        if history != () {
            data_set("ep." + rule.name, history);
            data_remove("ep." + editing);
        }
    }

    rules = kept;
    editing = "";
    ui_form_close();
    selected = global::RULES;
    data_set("selected", selected);
    draw_feeds(feeds, selected, rules.len());
    draw_buttons(selected);
    draw_rules(rules);
    ui_status("Rule '" + rule.name + "' saved. It runs on the next check.");
}

// The form was dismissed. Put back whatever was on screen before it.
fn on_ui_form_cancel(form_id) {
    editing = "";
    ui_form_close();
    draw_feeds(feeds, selected, rules.len());
    draw_buttons(selected);
    if selected == global::RULES {
        rules = rule_list();
        draw_rules(rules);
        ui_status(rules.len() + " rule(s). Click one to edit it.");
        return;
    }
    draw_items(items);
}

fn on_ui_menu(id) {
    if id == "open" {
        ui_show();
    } else if id == "settings" {
        ui_show();
        show_settings_form();
    } else if id == "refresh" {
        ui_show();
        items = load_one(selected, setting_int("articles", 50));
        draw_items(items);
    } else if id == "runrules" {
        ui_show();
        feeds = lines_of("feeds");
        rules = rule_list();
        ui_status("Checking " + feeds.len() + " feed(s) against " + rules.len() + " rule(s)...");
        let added = run_rules(feeds, rules, setting_int("articles", 50),
                              setting_on("repacks", false));
        ui_status("Auto-download added " + added + " torrent(s).");
    }
}

// Configure, on this plugin's row in Preferences. The window IS the settings -
// the feeds and the rules are most of what there is to configure - so this
// opens it with the options form already up.
fn on_ui_configure() {
    ui_show();
    show_settings_form();
}

fn on_tick() {
    ticks += 1;
    if ticks < setting_int("interval", 15) { return; }
    ticks = 0;

    let limit = setting_int("articles", 50);

    // The rules sweep reads every feed, so it is also the refresh. Doing both
    // would fetch the selected feed twice every interval.
    feeds = lines_of("feeds");
    rules = rule_list();
    if setting_on("auto", true) && !rules.is_empty() {
        run_rules(feeds, rules, limit, setting_on("repacks", false));
    }

    // Not while a form is open: redrawing the lists underneath it is at best
    // pointless and at worst takes the window out from under someone typing.
    if editing != "" { return; }
    if selected != "" && selected != global::RULES {
        items = load_one(selected, limit);
        draw_items(items);
    }
}
