//! Who provides which action, which keys run them, and who hears which events.

use std::collections::{BTreeMap, HashMap, HashSet};

use thurm_proto::ext::{ActionInfo, Binding};

/// Who carries out an action or bound a key.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Provider {
    /// A `[[commands]]` entry: the daemon runs it itself.
    Config,
    /// A client of the extension API (an app, a plugin, a script), by its client id.
    Client(u64),
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub info: ActionInfo,
    pub provider: Provider,
}

#[derive(Default)]
pub struct Registry {
    actions: BTreeMap<String, Entry>,
    /// In the order they were bound: of two bindings of one key, the later one counts.
    bindings: Vec<(Binding, Provider)>,
    /// Per client, the events it hears (`*` for all).
    subscriptions: HashMap<u64, HashSet<String>>,
}

impl Registry {
    /// Replaces the config's actions and keys with `commands`' (name, key).
    pub fn set_config(&mut self, commands: &[thurm_config::CommandConfig]) {
        self.actions.retain(|_, e| e.provider != Provider::Config);
        self.bindings.retain(|(_, p)| *p != Provider::Config);
        for c in commands {
            // A client's action of the same name gives way: the file is the user's own word.
            self.actions.insert(
                c.name.clone(),
                Entry {
                    info: ActionInfo {
                        name: c.name.clone(),
                        category: "Commands".into(),
                        source: "config".into(),
                        ..Default::default()
                    },
                    provider: Provider::Config,
                },
            );
            if !c.key.trim().is_empty() {
                // Config keys go first: anything bound later, by a plugin, wins over them.
                self.bindings.insert(
                    0,
                    (
                        Binding {
                            key: c.key.trim().to_owned(),
                            action: c.name.clone(),
                            source: "config".into(),
                            ..Default::default()
                        },
                        Provider::Config,
                    ),
                );
            }
        }
    }

    /// Adds `actions` for `client`, all or none: a name taken by the config or another client
    /// refuses the lot.
    pub fn register(
        &mut self,
        client: u64,
        source: &str,
        actions: Vec<ActionInfo>,
    ) -> Result<(), String> {
        let mut seen = HashSet::new();
        for a in &actions {
            if !thurm_proto::ext::valid_action_name(&a.name) {
                return Err(format!("{:?} is not a valid action name", a.name));
            }
            if !seen.insert(a.name.as_str()) {
                return Err(format!("{:?} is registered twice", a.name));
            }
            if let Some(e) = self.actions.get(&a.name)
                && e.provider != Provider::Client(client)
            {
                return Err(format!(
                    "the action {:?} is provided by {} already",
                    a.name, e.info.source
                ));
            }
        }
        for mut info in actions {
            info.source = source.to_owned();
            self.actions.insert(
                info.name.clone(),
                Entry {
                    info,
                    provider: Provider::Client(client),
                },
            );
        }
        Ok(())
    }

    /// Drops `names` of `client`'s actions; returns whether any went.
    pub fn unregister(&mut self, client: u64, names: &[String]) -> bool {
        let before = self.actions.len();
        for n in names {
            if self
                .actions
                .get(n)
                .is_some_and(|e| e.provider == Provider::Client(client))
            {
                self.actions.remove(n);
            }
        }
        self.actions.len() != before
    }

    pub fn bind(&mut self, client: u64, source: &str, mut binding: Binding) -> Result<(), String> {
        binding.key = binding.key.trim().to_owned();
        if binding.key.is_empty() {
            return Err("no key to bind".into());
        }
        if !thurm_proto::ext::valid_action_name(&binding.action) {
            return Err(format!("{:?} is not a valid action name", binding.action));
        }
        binding.source = source.to_owned();
        self.unbind(client, &binding.key);
        self.bindings.push((binding, Provider::Client(client)));
        Ok(())
    }

    pub fn unbind(&mut self, client: u64, key: &str) -> bool {
        let before = self.bindings.len();
        self.bindings
            .retain(|(b, p)| !(*p == Provider::Client(client) && b.key == key.trim()));
        self.bindings.len() != before
    }

    /// Forgets everything `client` provided and heard; returns whether actions or keys went.
    pub fn client_gone(&mut self, client: u64) -> bool {
        self.subscriptions.remove(&client);
        let before = (self.actions.len(), self.bindings.len());
        self.actions
            .retain(|_, e| e.provider != Provider::Client(client));
        self.bindings
            .retain(|(_, p)| *p != Provider::Client(client));
        before != (self.actions.len(), self.bindings.len())
    }

    pub fn get(&self, name: &str) -> Option<&Entry> {
        self.actions.get(name)
    }

    pub fn actions(&self) -> Vec<ActionInfo> {
        self.actions.values().map(|e| e.info.clone()).collect()
    }

    /// The binding that counts for each key (the last one made), in key order.
    pub fn bindings(&self) -> Vec<Binding> {
        let mut by_key: BTreeMap<String, Binding> = BTreeMap::new();
        for (b, _) in &self.bindings {
            by_key.insert(normalize_key(&b.key), b.clone());
        }
        by_key.into_values().collect()
    }

    pub fn subscribe(&mut self, client: u64, names: Vec<String>) -> Result<(), String> {
        for n in &names {
            if n != "*" && !n.contains('.') && !thurm_proto::ext::EVENTS.contains(&n.as_str()) {
                return Err(format!(
                    "unknown event {n:?} (known: {})",
                    thurm_proto::ext::EVENTS.join(", ")
                ));
            }
        }
        self.subscriptions.entry(client).or_default().extend(names);
        Ok(())
    }

    pub fn unsubscribe(&mut self, client: u64, names: &[String]) {
        if names.is_empty() {
            self.subscriptions.remove(&client);
        } else if let Some(s) = self.subscriptions.get_mut(&client) {
            for n in names {
                s.remove(n);
            }
        }
    }

    /// The clients that hear `event`.
    pub fn listeners(&self, event: &str) -> Vec<u64> {
        self.subscriptions
            .iter()
            .filter(|(_, s)| s.contains(event) || s.contains("*"))
            .map(|(c, _)| *c)
            .collect()
    }
}

/// A chord as a comparable string: modifiers in a fixed order, lower case (`Shift+Cmd+L` and
/// `cmd+shift+l` are one key).
pub fn normalize_key(key: &str) -> String {
    let parts: Vec<String> = key
        .split('+')
        .map(|p| p.trim().to_lowercase())
        .filter(|p| !p.is_empty())
        .collect();
    let Some((last, mods)) = parts.split_last() else {
        return String::new();
    };
    let rank = |m: &str| match m {
        "ctrl" | "control" => 0,
        "alt" | "opt" | "option" => 1,
        "shift" => 2,
        "cmd" | "command" | "super" => 3,
        _ => 4,
    };
    let mut mods: Vec<&String> = mods.iter().collect();
    mods.sort_by_key(|m| (rank(m), m.to_string()));
    mods.dedup();
    let mut out: Vec<&str> = mods.iter().map(|m| m.as_str()).collect();
    out.push(last);
    out.join("+")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn action(name: &str) -> ActionInfo {
        ActionInfo {
            name: name.into(),
            ..Default::default()
        }
    }

    fn command(name: &str, key: &str) -> thurm_config::CommandConfig {
        thurm_config::CommandConfig {
            name: name.into(),
            exec: vec!["true".into()],
            key: key.into(),
        }
    }

    #[test]
    fn a_taken_name_refuses_the_whole_registration() {
        let mut r = Registry::default();
        r.set_config(&[command("Deploy", "")]);
        r.register(1, "plugin:a", vec![action("a.one")]).unwrap();
        let e = r
            .register(2, "plugin:b", vec![action("b.one"), action("a.one")])
            .unwrap_err();
        assert!(e.contains("plugin:a"), "{e}");
        assert!(r.get("b.one").is_none());
        assert!(r.register(2, "plugin:b", vec![action("Deploy")]).is_err());
        // The same client registering again replaces its own.
        r.register(1, "plugin:a", vec![action("a.one")]).unwrap();
        assert_eq!(r.get("a.one").unwrap().info.source, "plugin:a");
    }

    #[test]
    fn a_client_leaving_takes_its_actions_keys_and_subscriptions() {
        let mut r = Registry::default();
        r.register(1, "app", vec![action("split_down")]).unwrap();
        r.bind(
            1,
            "app",
            Binding {
                key: "cmd+d".into(),
                action: "split_down".into(),
                ..Default::default()
            },
        )
        .unwrap();
        r.subscribe(1, vec!["*".into()]).unwrap();
        assert_eq!(r.listeners("bell"), vec![1]);
        assert!(r.client_gone(1));
        assert!(r.actions().is_empty());
        assert!(r.bindings().is_empty());
        assert!(r.listeners("bell").is_empty());
    }

    #[test]
    fn the_last_binding_of_a_key_counts_and_the_earlier_comes_back() {
        let mut r = Registry::default();
        r.set_config(&[command("Deploy", "cmd+shift+d")]);
        let bind = |r: &mut Registry, c, key: &str, action: &str| {
            r.bind(
                c,
                "p",
                Binding {
                    key: key.into(),
                    action: action.into(),
                    ..Default::default()
                },
            )
            .unwrap()
        };
        bind(&mut r, 2, "Shift+Cmd+D", "other");
        assert_eq!(r.bindings().len(), 1);
        assert_eq!(r.bindings()[0].action, "other");
        r.client_gone(2);
        assert_eq!(r.bindings()[0].action, "Deploy");
        // Reloading the config keeps a plugin's binding on top.
        bind(&mut r, 3, "cmd+shift+d", "third");
        r.set_config(&[command("Deploy", "cmd+shift+d")]);
        assert_eq!(r.bindings()[0].action, "third");
    }

    #[test]
    fn subscriptions_check_their_events() {
        let mut r = Registry::default();
        assert!(r.subscribe(1, vec!["agent_finshed".into()]).is_err());
        r.subscribe(1, vec!["agent_finished".into(), "git.changed".into()])
            .unwrap();
        assert_eq!(r.listeners("git.changed"), vec![1]);
        r.unsubscribe(1, &["git.changed".into()]);
        assert!(r.listeners("git.changed").is_empty());
        assert_eq!(r.listeners("agent_finished"), vec![1]);
    }

    #[test]
    fn keys_compare_whatever_their_spelling() {
        assert_eq!(normalize_key("Shift+Cmd+L"), "shift+cmd+l");
        assert_eq!(normalize_key("cmd+shift+l"), "shift+cmd+l");
        assert_eq!(normalize_key("ctrl+alt+x"), normalize_key("alt+ctrl+X"));
    }
}
