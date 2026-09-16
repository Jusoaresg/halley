# Halley XWM initialization patch

Source: https://github.com/Smithay/smithay
Revision: ff5fa7df392cecfba049ffed55cdaa4e98a8e7ef (MIT; see LICENSE.txt).

This source snapshot is patched through Cargo so builds do not depend on changes
in a developer's Cargo cache. The upstream workspace member list is omitted.

The XWM initializes newly created surfaces on one property-reading worker.
Surfaces remain unpublished until initialization finishes. Subsequent X11 events
are queued and replayed in order, so mapping and destruction cannot overtake
initialization. The compositor event loop can continue handling Wayland clients,
input, and rendering while the X server takes time to answer property queries.
CreateNotify geometry is used directly instead of requesting the same geometry
synchronously for every newly created window.

This addresses the captured update_properties/update_motif_hints wait. Other
synchronous XWM request paths are not converted by this patch.
