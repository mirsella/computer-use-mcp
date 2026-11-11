#!/usr/bin/env python3
"""Synthetic GUI receiver for private Wayland tests. No desktop automation here."""

import argparse
import json
from pathlib import Path

import gi

gi.require_version("Gtk", "3.0")
gi.require_version("Gdk", "3.0")
from gi.repository import Gdk, GLib, Gtk  # noqa: E402


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--task", choices=("easy", "medium", "hard", "visual"), required=True
    )
    parser.add_argument("--state", type=Path, required=True)
    args = parser.parse_args()
    if Gdk.Display.get_default().__class__.__name__ != "GdkWaylandDisplay":
        raise RuntimeError("Fixture requires native Wayland, never X11")
    GLib.set_prgname("desktop-bench")
    GLib.set_application_name("Desktop Benchmark")
    state = {
        "task": args.task,
        "success": False,
        "events": [],
        "stage": "initial",
        "wrong_submissions": 0,
    }

    def publish(event):
        state["events"].append(event)
        temporary = args.state.with_suffix(".tmp")
        temporary.write_text(json.dumps(state, ensure_ascii=False))
        temporary.replace(args.state)

    window = Gtk.Window(title="Desktop Benchmark")
    window.set_default_size(900, 620)
    window.connect("destroy", Gtk.main_quit)
    box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=10, margin=16)
    window.add(box)

    def button(label, callback, parent=box):
        widget = Gtk.Button(label=label)
        widget.connect("clicked", callback)
        parent.pack_start(widget, False, False, 0)
        return widget

    def entry(label):
        widget = Gtk.Entry()
        widget.get_accessible().set_name(label)
        box.pack_start(Gtk.Label(label=label), False, False, 0)
        box.pack_start(widget, False, False, 0)
        return widget

    def complete(event):
        state["success"] = True
        state["stage"] = "complete"
        box.pack_start(Gtk.Label(label="Task complete"), False, False, 0)
        box.show_all()
        publish(event)

    def large_list():
        store = Gtk.ListStore(str)
        for index in range(2000):
            store.append([f"Inventory item {index:04d}"])
        tree = Gtk.TreeView(model=store)
        tree.append_column(
            Gtk.TreeViewColumn("Inventory", Gtk.CellRendererText(), text=0)
        )
        scroll = Gtk.ScrolledWindow()
        scroll.set_size_request(700, 380)
        scroll.add(tree)
        box.pack_start(scroll, True, True, 0)

    if args.task == "easy":
        message = entry("Message")

        def commit(_):
            state["message"] = message.get_text()
            if state["message"] == "Wayland benchmark 42":
                complete("commit")
            else:
                state["wrong_submissions"] += 1
                publish("wrong-message")

        button("Commit message", commit)
    elif args.task == "medium":
        large_list()
        button("Confirm inventory batch", lambda _: complete("inventory-confirmed"))
    elif args.task == "hard":
        large_list()

        def shipment(_):
            for child in box.get_children():
                child.destroy()
            state["stage"] = "form"
            recipient = entry("Recipient")
            note = entry("Dispatch note")
            express = Gtk.CheckButton(label="Express delivery")
            box.pack_start(express, False, False, 0)

            def review(_):
                state.update(
                    recipient=recipient.get_text(),
                    note=note.get_text(),
                    express=express.get_active(),
                )
                if (state["recipient"], state["note"], state["express"]) != (
                    "Ada Lovelace",
                    "café Δ 42",
                    True,
                ):
                    state["wrong_submissions"] += 1
                    publish("wrong-shipment")
                    return
                state["stage"] = "confirmation"
                dialog = Gtk.Dialog(
                    title="Confirm shipment", transient_for=window, modal=True
                )
                dialog.add_button("Cancel", Gtk.ResponseType.CANCEL)
                dialog.add_button("Confirm shipment", Gtk.ResponseType.OK)
                dialog.get_content_area().add(
                    Gtk.Label(label="Ada Lovelace — café Δ 42 — Express delivery")
                )

                def response(widget, response_id):
                    widget.destroy()
                    if response_id == Gtk.ResponseType.OK:
                        complete("shipment-confirmed")
                    else:
                        state["stage"] = "form"
                        publish("confirmation-cancelled")

                dialog.connect("response", response)
                dialog.show_all()
                publish("reviewed")

            button("Review shipment", review)
            box.show_all()
            publish("shipment-started")

        button("Start shipment", shipment)
    else:
        canvas = Gtk.DrawingArea()
        canvas.get_accessible().set_name("Drawing canvas (no semantic controls)")
        canvas.set_can_focus(True)
        canvas.add_events(
            Gdk.EventMask.BUTTON_PRESS_MASK | Gdk.EventMask.KEY_PRESS_MASK
        )
        box.pack_start(canvas, True, True, 0)
        state.update(clicked=False, typed="")

        def draw(widget, context):
            context.set_source_rgb(0.96, 0.96, 0.96)
            context.paint()
            context.select_font_face("Sans")
            context.set_font_size(23)
            context.set_source_rgb(0.1, 0.1, 0.1)
            context.move_to(25, 45)
            context.show_text("Click GREEN, type Wayland test42, then Enter")
            for x, color, label in [
                (60, (0.8, 0.15, 0.12), "RED"),
                (320, (0.1, 0.35, 0.8), "BLUE"),
                (580, (0.1, 0.65, 0.2), "GREEN"),
            ]:
                context.set_source_rgb(*color)
                context.rectangle(x, 120, 180, 130)
                context.fill()
                context.set_source_rgb(1, 1, 1)
                context.move_to(x + 35, 195)
                context.show_text(label)
            context.set_source_rgb(0.1, 0.1, 0.1)
            context.move_to(25, 330)
            context.show_text(f"Selected green: {state['clicked']}")
            context.move_to(25, 380)
            context.show_text(f"Typed: {state['typed']}")

        def click(widget, event):
            state["clicked"] = 580 <= event.x < 760 and 120 <= event.y < 250
            widget.grab_focus()
            publish("canvas-click")
            widget.queue_draw()
            return True

        def key(widget, event):
            if event.keyval in (Gdk.KEY_Return, Gdk.KEY_KP_Enter):
                if state["clicked"] and state["typed"] == "Wayland test42":
                    complete("canvas-enter")
                else:
                    state["wrong_submissions"] += 1
                    publish("wrong-canvas")
            elif event.keyval == Gdk.KEY_BackSpace:
                state["typed"] = state["typed"][:-1]
            else:
                scalar = Gdk.keyval_to_unicode(event.keyval)
                if scalar and chr(scalar).isprintable():
                    state["typed"] += chr(scalar)
            widget.queue_draw()
            publish("canvas-key")
            return True

        canvas.connect("draw", draw)
        canvas.connect("button-press-event", click)
        canvas.connect("key-press-event", key)
    window.show_all()

    def ready():
        publish("ready")
        print("READY", flush=True)
        return False

    GLib.idle_add(ready)
    Gtk.main()


if __name__ == "__main__":
    main()
