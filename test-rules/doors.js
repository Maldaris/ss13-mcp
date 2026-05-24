// Door and access validation rules

rule("airlock-not-in-space", {
  anchor: "/obj/machinery/door/airlock",
  severity: "error",
  message: "Airlock at ({x},{y},{z}) is placed in space area",
  check: function(obj, ctx) {
    var area = ctx.areaOf(obj.x, obj.y, obj.z);
    if (area && area.indexOf("/area/space") === 0) {
      return false;
    }
    return true;
  }
});



rule("airlock-borders-two-areas", {
  anchor: "/obj/machinery/door/airlock",
  severity: "info",
  message: "Airlock at ({x},{y},{z}) does not border two different areas",
  check: function(obj, ctx) {
    // Get dir to determine orientation (N/S vs E/W)
    var dir = ctx.varOf(obj, "dir", 2);
    var areas = {};
    var adj = ctx.adjacent(obj.x, obj.y, obj.z);
    for (var i = 0; i < adj.length; i++) {
      // Only look at turf/area tiles
      if (adj[i].path.indexOf("/area/") === 0) continue;
      var areaAt = ctx.areaOf(adj[i].x, adj[i].y, adj[i].z);
      if (areaAt) areas[areaAt] = true;
    }
    var areaCount = Object.keys(areas).length;
    // Airlocks typically border 2+ areas (department boundaries)
    // Single-area airlocks aren't errors, just informational
    if (areaCount < 2) return false;
    return true;
  }
});
