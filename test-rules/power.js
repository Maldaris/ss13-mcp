// Power infrastructure validation rules

// APCs auto-create terminals at runtime, so we check for cable instead.
// An APC needs cable on its tile or directly behind it (where the terminal spawns).
rule("apc-needs-cable", {
  anchor: "/obj/machinery/power/apc",
  severity: "error",
  message: "APC at ({x},{y},{z}) has no cable on or behind it",
  check: function(obj, ctx) {
    // Check cable on the APC's own tile
    var here = ctx.at(obj.x, obj.y, obj.z);
    for (var i = 0; i < here.length; i++) {
      if (ctx.isType(here[i], "/obj/structure/cable")) return true;
    }
    // Check cable behind the APC (where terminal spawns)
    var dir = ctx.inferDir(obj);
    var delta = ctx.dirToDelta(dir);
    var behind = ctx.at(obj.x + delta.dx, obj.y + delta.dy, obj.z || 1);
    for (var i = 0; i < behind.length; i++) {
      if (ctx.isType(behind[i], "/obj/structure/cable")) return true;
    }
    return false;
  }
});

// SMES units should be connected to cable
rule("smes-needs-cable", {
  anchor: "/obj/machinery/power/smes",
  severity: "error",
  message: "SMES at ({x},{y},{z}) has no cable connection",
  check: function(obj, ctx) {
    var here = ctx.at(obj.x, obj.y, obj.z);
    for (var i = 0; i < here.length; i++) {
      if (ctx.isType(here[i], "/obj/structure/cable")) return true;
    }
    // Also check adjacent tiles
    var adj = ctx.adjacent(obj.x, obj.y, obj.z);
    for (var i = 0; i < adj.length; i++) {
      if (ctx.isType(adj[i], "/obj/structure/cable")) return true;
    }
    return false;
  }
});

// Solar trackers need cable
rule("solar-tracker-needs-cable", {
  anchor: "/obj/machinery/power/tracker",
  severity: "warning",
  message: "Solar tracker at ({x},{y},{z}) has no cable",
  check: function(obj, ctx) {
    var here = ctx.at(obj.x, obj.y, obj.z);
    for (var i = 0; i < here.length; i++) {
      if (ctx.isType(here[i], "/obj/structure/cable")) return true;
    }
    return false;
  }
});

// Solar panels need cable
rule("solar-panel-needs-cable", {
  anchor: "/obj/machinery/power/solar",
  severity: "warning",
  message: "Solar panel at ({x},{y},{z}) has no cable",
  check: function(obj, ctx) {
    var here = ctx.at(obj.x, obj.y, obj.z);
    for (var i = 0; i < here.length; i++) {
      if (ctx.isType(here[i], "/obj/structure/cable")) return true;
    }
    return false;
  }
});
