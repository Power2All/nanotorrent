/* Strict mode: stop rather than leak.

   Off by default. On, the client refuses to run anything that cannot be
   covered by the proxy or the bound interface, and refuses to start at all if
   the protection it was told to use is not there.

   Concretely, with a SOCKS proxy and no bound interface: DHT and uTP stop,
   because neither has any proxy code and both are UDP; local peer discovery
   stops, because it is LAN multicast; UPnP stops, because it talks to the
   router; and IPv6 is switched off, because this client cannot verify which
   family the proxy is reached over.

   With an interface bound instead, DHT and uTP keep running - they are inside
   the tunnel like everything else - and only the two that talk to the local
   network are stopped. IPv6 is switched off only when the tunnel itself
   carries no IPv6, which is the case where a v6 socket would route around the
   binding through the ordinary interface.

   The point of the setting is the refusal. Binding sockets or routing through
   a proxy is a preference on its own: the components that cannot honour it
   carry on regardless, and on a torrent client those are the loudest ones. */

INSERT INTO setting (key, value, default_value)
VALUES ('network.strict', NULL, 'false');
