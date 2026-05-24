// Wall-mounted object placement validation
//
// Convention: directional wall-mounted objects (APC, light, fire alarm, etc.)
// are placed on the FLOOR tile adjacent to the wall they're mounted on.
// "directional/north" means mounted on the north wall, placed on the floor south of it.
//
// Valid: APC/directional/north on a floor tile, wall exists to the north
// Invalid: APC/directional/north on a wall tile
// Invalid: APC/directional/north with no wall to the north

// Wall-mounted objects "attach" to whatever is in the direction they face.
// In the layout below, the light is mounted *on the door tile to its north*,
// which is structurally invalid — the mount has nothing to fasten to.
//
//   . L .
//   w D w     L = light (directional/north → faces north)
//   . . .     D = door, w = wall
//
// Valid mount surfaces are walls (/turf/closed). Doors, windows, and open
// space don't count. Some types legitimately mount on non-wall surfaces
// (door buttons, window-doors); those are handled by wall-mount-no-wall-behind
// which uses an explicit exempt list.
rule("wall-mount-attached-to-door", {
  anchor: "/obj/machinery",
  severity: "error",
  message: "Wall-mount at ({x},{y},{z}) is attached to a door — should face a wall instead",
  check: function(obj, ctx) {
    if (obj.path.indexOf("/directional/") === -1) return true;
    if (ctx.isType(obj, "/obj/machinery/door")) return true;

    // Some directional types legitimately face non-wall surfaces. Door
    // buttons in particular mount on the wall *next to* the door they
    // control, which often puts them facing the door itself.
    var exempt = [
      "/obj/machinery/button",
      "/obj/machinery/door/window",
    ];
    for (var i = 0; i < exempt.length; i++) {
      if (ctx.isType(obj, exempt[i])) return true;
    }

    // Compute the tile the mount faces
    var dir = ctx.inferDir(obj);
    var delta = ctx.dirToDelta(dir);
    var fx = obj.x + delta.dx;
    var fy = obj.y + delta.dy;
    var fz = obj.z || 1;
    var facing = ctx.at(fx, fy, fz);

    // If facing tile contains a true door (airlock, firedoor), the mount
    // is "on the door". Poddoors are shutters/blast doors that sit on
    // reinforced windows — they're functionally a wall when closed, so
    // mounting next to one is valid.
    for (var i = 0; i < facing.length; i++) {
      if (ctx.isType(facing[i], "/obj/machinery/door/poddoor")) continue;
      if (ctx.isType(facing[i], "/obj/machinery/door")) {
        return false;
      }
    }
    return true;
  }
});

// Wall-mounted objects must not be on a wall tile
rule("wall-mount-on-wall-tile", {
  anchor: "/obj/machinery",
  severity: "error",
  message: "Wall-mounted object at ({x},{y},{z}) is on a wall tile — should be on the adjacent floor tile",
  check: function(obj, ctx) {
    // Only check directional wall-mounted types
    if (obj.path.indexOf("/directional/") === -1) return true;

    var here = ctx.at(obj.x, obj.y, obj.z);
    for (var i = 0; i < here.length; i++) {
      if (ctx.isType(here[i], "/turf/closed")) {
        return false;
      }
    }
    return true;
  }
});

// Wall-mounted objects must face a wall (strict subset — excludes types
// commonly mounted on windows, reinforced glass, etc.)
rule("wall-mount-no-wall-behind", {
  anchor: "/obj/machinery",
  severity: "warning",
  message: "Wall-mounted object at ({x},{y},{z}) has no wall in the direction it faces",
  check: function(obj, ctx) {
    if (obj.path.indexOf("/directional/") === -1) return true;

    // Exclude types that legitimately mount on non-wall surfaces
    var exempt = [
      "/obj/machinery/door/window",    // window-doors mount on glass
      "/obj/machinery/shower",          // showers mount on shower walls
      "/obj/machinery/flasher",         // flashers mount on various surfaces
      "/obj/machinery/button",          // buttons mount near doors/windows
      "/obj/machinery/door/firedoor",   // firedoors in open corridors
    ];
    for (var i = 0; i < exempt.length; i++) {
      if (ctx.isType(obj, exempt[i])) return true;
    }

    return ctx.wallBehind(obj) !== null;
  }
});

// APC-specific: redundant with above but explicit for clarity in reports
rule("apc-on-wall-tile", {
  anchor: "/obj/machinery/power/apc",
  severity: "error",
  message: "APC at ({x},{y},{z}) is on a wall tile — APCs go on the floor tile adjacent to the wall",
  check: function(obj, ctx) {
    var here = ctx.at(obj.x, obj.y, obj.z);
    for (var i = 0; i < here.length; i++) {
      if (ctx.isType(here[i], "/turf/closed")) {
        return false;
      }
    }
    return true;
  }
});
