"""Versioned, seeded GUI goals shared by the benchmark and independent receivers."""

import random

VERSION = "expanded-v1"
WORKLOADS = {
    "entry": ("GTK3", "semantic", "entry replacement"),
    "inventory": ("GTK3", "semantic", "large collection/footer"),
    "shipping": ("GTK3", "semantic", "Unicode form/modal"),
    "settings": ("GTK3", "semantic", "hidden tabs/duplicate labels"),
    "delayed": ("GTK3", "semantic", "asynchronous validation"),
    "search": ("GTK3", "semantic", "filter/select/reserve"),
    "cancel": ("GTK3", "semantic", "cancel destructive dialog/edit"),
    "qt_invoice": ("Qt6", "semantic", "combo/spin/form/modal"),
    "gtk4_form": ("GTK4", "semantic", "dropdown/check/form"),
    "kwrite": ("KWrite", "document", "real editor/save verification"),
    "canvas": ("GTK3", "visual", "pixel click/generated typing"),
    "drag": ("GTK3", "visual", "pixel drag/drop"),
    # Harder families added 2026-11-11; existing seeds keep their parameters.
    "menu": ("GTK3", "semantic", "menubar/submenu/export dialog"),
    "tree": ("GTK3", "semantic", "collapsed tree expansion/selection"),
    "editor": ("GTK3", "semantic", "second window/multi-line note/save"),
    "context": ("GTK3", "hybrid", "right-click row/context menu"),
    "slider": ("GTK3", "semantic", "slider and spin button exact values"),
    "checklist": ("GTK3", "semantic", "exact multi-toggle with pre-set decoys"),
    "celledit": ("GTK3", "hybrid", "in-place table cell edit"),
    "wizard": ("GTK3", "semantic", "multi-page flow/combo/remembered code"),
    "twin": ("GTK3", "semantic", "same-titled windows disambiguation"),
    "shortcut": ("GTK3", "hybrid", "keyboard-only reveal and submit"),
}
FEATURES = [
    "Audit log", "Backups", "Caching", "Dark mode", "Email digests",
    "File sync", "Guest access", "History", "Indexing", "Journaling",
    "Kiosk mode", "Live preview", "Metrics", "Notifications", "Offline mode",
    "Plugins", "Quotas", "Reminders", "Spell check", "Telemetry",
    "Updates", "Versioning", "Webhooks", "Zoom controls",
]


def workload(task, seed):
    if task not in WORKLOADS or not isinstance(seed, int) or seed < 1:
        raise ValueError("Unknown workload or invalid seed")
    toolkit, capability, label = WORKLOADS[task]
    rng = random.Random(f"{VERSION}:{task}:{seed}")
    params = {
        "text": rng.choice(["café Δ", "Zürich Ω", "naïve λ", "résumé β"])
        + f" {rng.randrange(100, 999)}",
        "recipient": rng.choice(["Ada Lovelace", "Grace Hopper", "Katherine Johnson"]),
        "rows": rng.choice([500, 2000, 5000]),
        "initial_checked": seed % 2 == 0,
        "delay_ms": rng.choice([550, 950, 1450]),
        "units": rng.randrange(2, 9),
        "choice": rng.choice(["Economy", "Standard", "Express"]),
        "sku": f"SKU-{rng.randrange(5000, 9999)}",
        "green_x": rng.choice([80, 320, 560]),
        "green_y": rng.choice([130, 220]),
        "report": f"Report {rng.choice([2021, 2022, 2023])}-Q{rng.randrange(1, 5)}",
        "row": rng.randrange(3, 10),
        "level": rng.choice([value for value in range(11, 90) if value != 50]),
        "copies": rng.randrange(12, 60),
        # First three are wanted (the first starts enabled); the last two
        # start enabled and must be disabled.
        "features": [FEATURES[index] for index in rng.sample(range(len(FEATURES)), 5)],
        "cell": str(rng.randrange(100, 999)),
        "plan": rng.choice(["Basic", "Team", "Enterprise"]),
        "region": rng.choice(["Europe", "Asia", "Americas", "Oceania"]),
        "code": str(rng.randrange(1000, 9999)),
        "chord": rng.choice(["Ctrl+Alt+J", "Ctrl+Shift+comma", "Ctrl+F7"]),
    }
    wanted = ", ".join(params["features"][:3])
    q = lambda value: repr(value)
    goals = {
        "entry": f"Replace Message with exactly {q(params['text'])}, then Commit message.",
        "inventory": "Confirm the inventory batch using the footer below the large inventory. Do not edit or select inventory rows.",
        "shipping": f"Start shipment. Set Recipient to {q(params['recipient'])}, Dispatch note to {q(params['text'])}, ensure Express delivery is enabled, Review shipment, and confirm the dialog.",
        "settings": f"Open the Delivery tab. Set Reference to {q(params['text'])}, ensure Delivery alerts is enabled, and Save that tab. Do not save or change the Personal tab.",
        "delayed": f"Set Validation code to {q(params['text'])}, click Validate code once, wait for validation to finish, then Commit validated code. Do not submit before validation completes.",
        "search": f"Filter the catalogue to {params['sku']}, select that exact item, then Reserve selected item.",
        "cancel": f"Open Delete record, CANCEL the confirmation without deleting anything, then replace Description with {q(params['text'])} and Apply description.",
        "qt_invoice": f"Set Customer to {q(params['recipient'])}, Units to {params['units']}, Service to {params['choice']}, ensure Insured is enabled, then Review invoice and Confirm invoice.",
        "gtk4_form": f"Set Reference to {q(params['text'])}, Service to {params['choice']}, ensure Notifications is enabled, then Apply preferences.",
        "kwrite": f"In KWrite replace the existing document with exactly these two lines, then save that existing document: first line {q(params['text'])}; second line {q(params['recipient'])}. Keep one newline between the lines and no trailing newline. Do not write other files.",
        "canvas": f"On the drawing canvas click the GREEN rectangle, use generated keyboard input to type exactly {q(params['text'])}, then press Enter. There are no semantic controls; use actual screenshot pixels, never accessibility bounds.",
        "drag": "On the drawing canvas drag the solid GREEN square into the outlined DROP TARGET and release it there. There are no semantic controls; use actual screenshot pixels, never accessibility bounds.",
        "menu": f"Using the File menu, open Export, choose Export as CSV…, set File name to {q(params['text'])} in the dialog, then press Export. Do not export JSON.",
        "tree": f"Expand the Archived folder in the report tree, select exactly {q(params['report'])} under Archived, then click Open selected report. Do not open any other report.",
        "editor": f"Click Open note editor. In the Note editor window replace the note with exactly two lines: first line {q(params['text'])}, second line {q(params['recipient'])}, one newline between them, no trailing newline. Click Save note, then click Finish review in the main window.",
        "context": f"Right-click the catalogue row {params['sku']} and choose Archive item from its context menu. Do not delete anything and do not archive another row.",
        "slider": f"Set the Volume slider to exactly {params['level']} and Copies to exactly {params['copies']}, then click Apply levels.",
        "checklist": f"In the feature list make exactly these three features enabled and every other feature disabled: {wanted}. Some features are already enabled. Then click Apply selection.",
        "celledit": f"In the Stock table change the Quantity of row {params['sku']} to exactly {params['cell']} by editing that cell in place and confirming with Enter, then click Save quantities. Do not change any other row.",
        "wizard": f"Complete the setup wizard. On the first page choose the {params['plan']} plan and note the confirmation code shown there. On the second page choose region {params['region']} and accept the terms. On the last page enter that confirmation code and click Finish setup.",
        "twin": f"Two windows have the same title. Find the one showing order {params['sku']} and click Approve order in that window only. Do not approve or reject the other order.",
        "shortcut": f"The token panel is hidden and there is no button for it. Press {params['chord']} in the benchmark window to reveal it, set Token to exactly {q(params['text'])}, then press Enter in the Token field to submit.",
    }
    return {
        "version": VERSION,
        "task": task,
        "seed": seed,
        "toolkit": toolkit,
        "capability": capability,
        "label": label,
        "app": "KWrite" if task == "kwrite" else "Desktop Benchmark",
        "goal": goals[task],
        "params": params,
        "timeout_cap": 90 if capability == "visual" else 180,
    }
