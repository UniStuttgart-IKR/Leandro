# SPDX-License-Identifier: MIT
# Force Cycles onto CUDA and say what it picked -- a render that silently
# falls back to CPU is not a GPU benchmark, and Blender does that quietly.
import bpy
prefs = bpy.context.preferences.addons["cycles"].preferences
prefs.compute_device_type = "CUDA"
prefs.get_devices()
names = []
for d in prefs.devices:
    d.use = (d.type == "CUDA")
    if d.use:
        names.append(d.name)
print("CYCLES-DEVICES:", names)
scene = bpy.context.scene
scene.cycles.device = "GPU"
scene.render.resolution_percentage = 100
scene.cycles.samples = 200
