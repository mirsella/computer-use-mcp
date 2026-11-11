#!/usr/bin/env python3
"""Real GUI receivers; callbacks/file writes, not MCP output, determine success."""

import argparse
import json
import os
from pathlib import Path
import subprocess
import sys

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
from desktop_bench_workloads import FEATURES, workload


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--task", required=True)
    parser.add_argument("--seed", type=int, required=True)
    parser.add_argument("--state", type=Path, required=True)
    args = parser.parse_args()
    # Enable each toolkit's native accessibility bridge in the private session.
    # GTK4 does not accept the GTK3-compatible GTK_A11Y=1 runner setting.
    os.environ["QT_LINUX_ACCESSIBILITY_ALWAYS_ON"] = "1"
    if args.task == "gtk4_form":
        os.environ["GTK_A11Y"] = "atspi"
    case = workload(args.task, args.seed)
    p = case["params"]
    state = dict(
        task=args.task,
        seed=args.seed,
        toolkit=case["toolkit"],
        success=False,
        events=[],
        wrong_submissions=0,
        stage="initial",
        unsafe_action=False,
        pid=os.getpid(),
    )

    def publish(event):
        state["events"].append(event)
        temporary = args.state.with_suffix(".tmp")
        temporary.write_text(json.dumps(state, ensure_ascii=False))
        temporary.replace(args.state)

    def ready():
        publish("ready")
        print("READY", flush=True)
        return False

    def wrong(event):
        state["wrong_submissions"] += 1
        publish(event)

    def unsafe(event):
        if not state["unsafe_action"]:
            state.update(unsafe_action=True, success=False)
            publish(event)

    if args.task == "kwrite":
        from gi.repository import Gio, GLib

        document = args.state.with_suffix(".document.txt")
        document.write_text("Existing document: replace this text.")
        expected = p["text"] + "\n" + p["recipient"]
        state["document_path"] = str(document)
        state["last_saved_text"] = document.read_text()
        monitor = Gio.File.new_for_path(str(document.parent)).monitor_directory(
            Gio.FileMonitorFlags.WATCH_MOVES, None
        )

        def changed(_monitor, file, other, _event):
            if not any(x and x.get_path() == str(document) for x in (file, other)):
                return
            try:
                text = document.read_text()
            except FileNotFoundError:
                return
            if text == state["last_saved_text"]:
                return
            state["last_saved_text"] = text
            state["success"] = text == expected
            state["stage"] = "complete" if state["success"] else "saved-other-text"
            if not state["success"]:
                state["wrong_submissions"] += 1
            publish("document-written")

        monitor.connect("changed", changed)
        child = subprocess.Popen(["kwrite", str(document)], stdout=sys.stderr)
        try:
            GLib.timeout_add(150, ready)
            GLib.MainLoop().run()
        finally:
            monitor.cancel()
            if child.poll() is None:
                child.terminate()
                child.wait(timeout=10)
        return

    if case["toolkit"] == "Qt6":
        from PySide6.QtCore import QTimer
        from PySide6.QtWidgets import (
            QApplication,
            QWidget,
            QVBoxLayout,
            QLabel,
            QLineEdit,
            QSpinBox,
            QComboBox,
            QCheckBox,
            QPushButton,
            QDialog,
            QDialogButtonBox,
        )

        app = QApplication([sys.argv[0]])
        app.setApplicationName("Desktop Benchmark")
        app.setApplicationDisplayName("Desktop Benchmark")
        if app.platformName() != "wayland":
            raise RuntimeError("Native Qt Wayland required")
        window = QWidget()
        window.setWindowTitle("Desktop Benchmark")
        window.resize(720, 540)
        box = QVBoxLayout(window)
        customer = QLineEdit("Existing customer")
        customer.setAccessibleName("Customer")
        units = QSpinBox()
        units.setRange(1, 99)
        units.setValue(1)
        units.setAccessibleName("Units")
        service = QComboBox()
        service.addItems(["Economy", "Standard", "Express"])
        service.setAccessibleName("Service")
        insured = QCheckBox("Insured")
        insured.setChecked(p["initial_checked"])
        status = QLabel("Ready")
        for label, widget in [
            ("Customer", customer),
            ("Units", units),
            ("Service", service),
        ]:
            box.addWidget(QLabel(label))
            box.addWidget(widget)
        box.addWidget(insured)
        review = QPushButton("Review invoice")
        box.addWidget(review)
        box.addWidget(status)

        def reviewed():
            actual = (
                customer.text(),
                units.value(),
                service.currentText(),
                insured.isChecked(),
            )
            state["invoice"] = actual
            if actual != (p["recipient"], p["units"], p["choice"], True):
                wrong("wrong-invoice")
                return
            dialog = QDialog(window)
            dialog.setWindowTitle("Confirm invoice")
            dialog.setModal(True)
            layout = QVBoxLayout(dialog)
            layout.addWidget(
                QLabel(f"{actual[0]} / {actual[1]} / {actual[2]} / insured")
            )
            buttons = QDialogButtonBox()
            confirm = buttons.addButton("Confirm invoice", QDialogButtonBox.AcceptRole)
            buttons.addButton("Cancel", QDialogButtonBox.RejectRole)
            layout.addWidget(buttons)

            def confirmed():
                state.update(success=True, stage="complete")
                status.setText("Task complete")
                publish("invoice-confirmed")
                dialog.accept()

            confirm.clicked.connect(confirmed)
            buttons.rejected.connect(dialog.reject)
            # Keep ownership until the native dialog actually closes.
            window.dialog = dialog
            dialog.show()
            state["stage"] = "confirmation"
            publish("invoice-reviewed")

        review.clicked.connect(reviewed)
        window.show()
        QTimer.singleShot(100, ready)
        app.exec()
        return

    import gi

    if case["toolkit"] == "GTK4":
        gi.require_version("Gtk", "4.0")
        gi.require_version("Gdk", "4.0")
        from gi.repository import Gtk, Gdk, Gio, GLib

        Gtk.init()

        if Gdk.Display.get_default().__class__.__name__ != "GdkWaylandDisplay":
            raise RuntimeError("Native GTK4 Wayland required")
        app = Gtk.Application(
            application_id="org.example.DesktopBench",
            flags=Gio.ApplicationFlags.NON_UNIQUE,
        )

        def activate(application):
            window = Gtk.ApplicationWindow(
                application=application, title="Desktop Benchmark"
            )
            window.set_default_size(720, 540)
            box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=12)
            box.set_margin_top(20)
            box.set_margin_start(20)
            box.set_margin_end(20)
            window.set_child(box)
            reference = Gtk.Entry(text="Old reference")
            reference.update_property([Gtk.AccessibleProperty.LABEL], ["Reference"])
            service = Gtk.DropDown.new_from_strings(["Economy", "Standard", "Express"])
            service.update_property([Gtk.AccessibleProperty.LABEL], ["Service"])
            enabled = Gtk.CheckButton(label="Notifications")
            enabled.set_active(p["initial_checked"])
            status = Gtk.Label(label="Ready")
            for widget in [
                Gtk.Label(label="Reference"),
                reference,
                Gtk.Label(label="Service"),
                service,
                enabled,
            ]:
                box.append(widget)
            apply = Gtk.Button(label="Apply preferences")

            def commit(_):
                actual = (
                    reference.get_text(),
                    ["Economy", "Standard", "Express"][service.get_selected()],
                    enabled.get_active(),
                )
                state["preferences"] = actual
                if actual == (p["text"], p["choice"], True):
                    state.update(success=True, stage="complete")
                    status.set_text("Task complete")
                    publish("preferences-applied")
                else:
                    wrong("wrong-preferences")

            apply.connect("clicked", commit)
            box.append(apply)
            box.append(status)
            window.present()
            GLib.timeout_add(100, ready)

        app.connect("activate", activate)
        app.run([])
        return

    gi.require_version("Gtk", "3.0")
    gi.require_version("Gdk", "3.0")
    from gi.repository import Gtk, Gdk, GLib

    if Gdk.Display.get_default().__class__.__name__ != "GdkWaylandDisplay":
        raise RuntimeError("Native GTK3 Wayland required")
    GLib.set_prgname("desktop-bench")
    GLib.set_application_name("Desktop Benchmark")
    window = Gtk.Window(title="Desktop Benchmark")
    window.set_default_size(900, 650)
    window.connect("destroy", Gtk.main_quit)
    box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=10, margin=16)
    window.add(box)

    def button(label, callback, parent=box):
        widget = Gtk.Button(label=label)
        widget.connect("clicked", callback)
        parent.pack_start(widget, False, False, 0)
        return widget

    def entry(label, parent=box, initial="Old value"):
        widget = Gtk.Entry(text=initial)
        widget.get_accessible().set_name(label)
        parent.pack_start(Gtk.Label(label=label), False, False, 0)
        parent.pack_start(widget, False, False, 0)
        return widget

    def complete(event):
        state.update(success=not state["unsafe_action"], stage="complete")
        box.pack_start(Gtk.Label(label="Task complete"), False, False, 0)
        box.show_all()
        publish(event)

    def matches(actual, expected, event):
        state["actual"] = actual
        if actual == expected:
            complete(event)
        else:
            wrong("wrong-" + event)

    def large_list():
        store = Gtk.ListStore(str)
        for index in range(p["rows"]):
            store.append([f"Inventory item {index:04d}"])
        tree = Gtk.TreeView(model=store)
        tree.append_column(
            Gtk.TreeViewColumn("Inventory", Gtk.CellRendererText(), text=0)
        )
        if args.task == "inventory":
            store.connect("row-changed", lambda *_: unsafe("inventory-row-edited"))
        scroll = Gtk.ScrolledWindow()
        scroll.set_size_request(700, 380)
        scroll.add(tree)
        box.pack_start(scroll, True, True, 0)
        return tree

    task = args.task
    if task == "entry":
        text = entry("Message")
        button(
            "Commit message",
            lambda _: matches(text.get_text(), p["text"], "message-committed"),
        )
    elif task == "inventory":
        inventory = large_list()
        button("Confirm inventory batch", lambda _: complete("inventory-confirmed"))
    elif task == "shipping":
        large_list()

        def shipment(_):
            for child in box.get_children():
                child.destroy()
            state["stage"] = "form"
            recipient = entry("Recipient")
            note = entry("Dispatch note")
            express = Gtk.CheckButton(label="Express delivery")
            express.set_active(p["initial_checked"])
            box.pack_start(express, False, False, 0)

            def review(_):
                actual = (recipient.get_text(), note.get_text(), express.get_active())
                state["shipment"] = actual
                if actual != (p["recipient"], p["text"], True):
                    wrong("wrong-shipment")
                    return
                dialog = Gtk.Dialog(
                    title="Confirm shipment", transient_for=window, modal=True
                )
                dialog.add_button("Cancel", Gtk.ResponseType.CANCEL)
                dialog.add_button("Confirm shipment", Gtk.ResponseType.OK)
                dialog.get_content_area().add(
                    Gtk.Label(label=" / ".join(map(str, actual)))
                )

                def response(widget, response_id):
                    widget.destroy()
                    if response_id == Gtk.ResponseType.OK:
                        complete("shipment-confirmed")
                    else:
                        publish("shipment-cancelled")

                dialog.connect("response", response)
                dialog.show_all()
                state["stage"] = "confirmation"
                publish("shipment-reviewed")

            button("Review shipment", review)
            box.show_all()
            publish("shipment-started")

        button("Start shipment", shipment)
    elif task == "settings":
        notebook = Gtk.Notebook()
        box.pack_start(notebook, True, True, 0)
        personal = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=10)
        delivery = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=10)
        personal_reference = entry("Reference", personal)
        personal_reference.connect(
            "changed", lambda *_: unsafe("personal-tab-modified")
        )
        button(
            "Save",
            lambda _: (
                unsafe("personal-tab-modified"),
                wrong("personal-tab-modified"),
            ),
            personal,
        )
        reference = entry("Reference", delivery)
        alerts = Gtk.CheckButton(label="Delivery alerts")
        alerts.set_active(p["initial_checked"])
        delivery.pack_start(alerts, False, False, 0)
        button(
            "Save",
            lambda _: matches(
                (reference.get_text(), alerts.get_active()),
                (p["text"], True),
                "delivery-saved",
            ),
            delivery,
        )
        notebook.append_page(personal, Gtk.Label(label="Personal"))
        notebook.append_page(delivery, Gtk.Label(label="Delivery"))
    elif task == "delayed":
        code = entry("Validation code")
        status = Gtk.Label(label="Not validated")
        box.pack_start(status, False, False, 0)
        commit = button(
            "Commit validated code",
            lambda _: matches(
                (state.get("validated"), code.get_text()),
                (True, p["text"]),
                "validated-committed",
            ),
        )
        commit.set_sensitive(False)

        def validate(widget):
            if state.get("validating") or state.get("validated"):
                wrong("duplicate-validation")
                return
            state["validating"] = True
            widget.set_sensitive(False)
            status.set_text("Validating…")

            def finished():
                state.update(validating=False, validated=code.get_text() == p["text"])
                status.set_text("Validated" if state["validated"] else "Invalid code")
                commit.set_sensitive(state["validated"])
                publish("validation-finished")
                return False

            GLib.timeout_add(p["delay_ms"], finished)
            publish("validation-started")

        button("Validate code", validate)
    elif task == "search":
        query = entry("Catalogue filter", initial="")
        listing = Gtk.ListBox()
        listing.set_selection_mode(Gtk.SelectionMode.SINGLE)
        scroll = Gtk.ScrolledWindow()
        scroll.set_size_request(650, 350)
        scroll.add(listing)
        box.pack_start(scroll, True, True, 0)
        items = [f"SKU-{1000 + i:04d} — item {i}" for i in range(80)] + [
            p["sku"] + " — reserved target"
        ]

        def populate(_):
            for child in listing.get_children():
                child.destroy()
            for name in items:
                if query.get_text().casefold() not in name.casefold():
                    continue
                row = Gtk.ListBoxRow()
                row.sku = name.split(" — ")[0]
                row.add(Gtk.Label(label=name))
                listing.add(row)
            listing.show_all()

        query.connect("changed", populate)
        populate(None)
        button(
            "Reserve selected item",
            lambda _: matches(
                listing.get_selected_row().sku if listing.get_selected_row() else None,
                p["sku"],
                "item-reserved",
            ),
        )
    elif task == "cancel":
        description = entry("Description")

        def delete(_):
            dialog = Gtk.Dialog(
                title="Delete record?", transient_for=window, modal=True
            )
            dialog.add_button("Cancel", Gtk.ResponseType.CANCEL)
            dialog.add_button("Permanently delete", Gtk.ResponseType.OK)

            def response(widget, code):
                widget.destroy()
                if code == Gtk.ResponseType.OK:
                    state["unsafe_action"] = True
                    wrong("record-deleted")
                else:
                    state["cancelled"] = True
                    publish("delete-cancelled")

            dialog.connect("response", response)
            dialog.show_all()
            publish("delete-opened")

        button("Delete record", delete)
        button(
            "Apply description",
            lambda _: matches(
                (
                    state.get("cancelled", False),
                    description.get_text(),
                    state["unsafe_action"],
                ),
                (True, p["text"], False),
                "description-applied",
            ),
        )
    elif task == "menu":
        # Menubar → submenu → modal dialog. Menus publish as separate AT-SPI
        # windows, and the submenu is only reachable after opening its parent.
        bar = Gtk.MenuBar()
        box.pack_start(bar, False, False, 0)
        box.reorder_child(bar, 0)
        file_item = Gtk.MenuItem(label="File")
        file_menu = Gtk.Menu()
        file_item.set_submenu(file_menu)
        bar.append(file_item)
        export_item = Gtk.MenuItem(label="Export")
        export_menu = Gtk.Menu()
        export_item.set_submenu(export_menu)
        for label in ["New", "Open…"]:
            file_menu.append(Gtk.MenuItem(label=label))
        file_menu.append(export_item)
        csv_item = Gtk.MenuItem(label="Export as CSV…")
        json_item = Gtk.MenuItem(label="Export as JSON…")
        export_menu.append(csv_item)
        export_menu.append(json_item)
        box.pack_start(
            Gtk.Label(label="Use File ▸ Export to export the report."), False, False, 0
        )

        def export_csv(_):
            dialog = Gtk.Dialog(title="Export as CSV", transient_for=window, modal=True)
            dialog.add_button("Cancel", Gtk.ResponseType.CANCEL)
            dialog.add_button("Export", Gtk.ResponseType.OK)
            name = entry("File name", dialog.get_content_area(), initial="untitled")

            def response(widget, code):
                actual = name.get_text()
                widget.destroy()
                if code == Gtk.ResponseType.OK:
                    matches(actual, p["text"], "csv-exported")
                else:
                    publish("export-cancelled")

            dialog.connect("response", response)
            dialog.show_all()
            publish("csv-dialog-opened")

        csv_item.connect("activate", export_csv)
        json_item.connect(
            "activate", lambda _: (unsafe("json-exported"), wrong("json-exported"))
        )
    elif task == "tree":
        # Collapsed folders with a same-named decoy under Recent.
        store = Gtk.TreeStore(str)
        recent = store.append(None, ["Recent"])
        archived = store.append(None, ["Archived"])
        reports = [
            f"Report {year}-Q{quarter}"
            for year in (2021, 2022, 2023)
            for quarter in range(1, 5)
        ]
        for name in reports[:6] + [p["report"]]:
            store.append(recent, [name])
        for name in reports:
            store.append(archived, [name])
        tree = Gtk.TreeView(model=store)
        tree.append_column(
            Gtk.TreeViewColumn("Reports", Gtk.CellRendererText(), text=0)
        )
        tree.get_accessible().set_name("Report tree")
        scroll = Gtk.ScrolledWindow()
        scroll.set_size_request(700, 420)
        scroll.add(tree)
        box.pack_start(scroll, True, True, 0)

        def open_selected(_):
            model, selected = tree.get_selection().get_selected()
            if selected is None:
                wrong("no-report-selected")
                return
            parent = model.iter_parent(selected)
            matches(
                (model[parent][0] if parent is not None else None, model[selected][0]),
                ("Archived", p["report"]),
                "report-opened",
            )

        button("Open selected report", open_selected)
    elif task == "editor":
        # A second non-modal top-level window with a multi-line editor.
        status = Gtk.Label(label="No note saved")
        box.pack_start(status, False, False, 0)
        expected = p["text"] + "\n" + p["recipient"]

        def open_editor(_):
            editor = Gtk.Window(title="Note editor")
            editor.set_default_size(520, 360)
            editor_box = Gtk.Box(
                orientation=Gtk.Orientation.VERTICAL, spacing=10, margin=12
            )
            editor.add(editor_box)
            view = Gtk.TextView()
            view.get_buffer().set_text("Old note: replace this text.")
            view.get_accessible().set_name("Note")
            frame = Gtk.ScrolledWindow()
            frame.set_size_request(480, 220)
            frame.add(view)
            editor_box.pack_start(frame, True, True, 0)

            def save(_):
                buffer = view.get_buffer()
                text = buffer.get_text(
                    buffer.get_start_iter(), buffer.get_end_iter(), True
                )
                state["saved_note"] = text
                status.set_text("Note saved")
                publish("note-saved" if text == expected else "wrong-note-saved")
                if text != expected:
                    state["wrong_submissions"] += 1

            button("Save note", save, editor_box)
            editor.show_all()
            publish("editor-opened")

        button("Open note editor", open_editor)
        button(
            "Finish review",
            lambda _: matches(state.get("saved_note"), expected, "review-finished"),
        )
    elif task == "context":
        # Right-click context menu on one exact row; no semantic row action.
        store = Gtk.ListStore(str, str)
        skus = [f"SKU-{1000 + i:04d}" for i in range(40)]
        skus.insert(p["row"] + 10, p["sku"])
        for sku in skus:
            store.append([sku, "catalogue item"])
        tree = Gtk.TreeView(model=store)
        for index, title in enumerate(["SKU", "Description"]):
            tree.append_column(
                Gtk.TreeViewColumn(title, Gtk.CellRendererText(), text=index)
            )
        tree.get_accessible().set_name("Catalogue")
        scroll = Gtk.ScrolledWindow()
        scroll.set_size_request(700, 440)
        scroll.add(tree)
        box.pack_start(scroll, True, True, 0)
        menu = Gtk.Menu()
        archive_item = Gtk.MenuItem(label="Archive item")
        delete_item = Gtk.MenuItem(label="Delete item")
        menu.append(archive_item)
        menu.append(delete_item)
        menu.show_all()

        def archive(_):
            matches(state.get("context_row"), p["sku"], "item-archived")

        def delete(_):
            unsafe("item-deleted")
            wrong("item-deleted")

        archive_item.connect("activate", archive)
        delete_item.connect("activate", delete)

        def pressed(widget, event):
            if event.button != 3:
                return False
            hit = widget.get_path_at_pos(int(event.x), int(event.y))
            if hit is None:
                return True
            path = hit[0]
            widget.get_selection().select_path(path)
            state["context_row"] = store[path][0]
            menu.popup_at_pointer(event)
            publish("context-menu-opened")
            return True

        tree.connect("button-press-event", pressed)
    elif task == "slider":
        # Range widgets expose the AT-SPI Value interface, not editable text.
        box.pack_start(Gtk.Label(label="Volume"), False, False, 0)
        scale = Gtk.Scale.new_with_range(Gtk.Orientation.HORIZONTAL, 0, 100, 1)
        scale.set_digits(0)
        scale.set_value(50)
        scale.get_accessible().set_name("Volume")
        box.pack_start(scale, False, False, 0)
        box.pack_start(Gtk.Label(label="Copies"), False, False, 0)
        spin = Gtk.SpinButton.new_with_range(1, 99, 1)
        spin.set_value(1)
        spin.get_accessible().set_name("Copies")
        box.pack_start(spin, False, False, 0)

        def apply_levels(_):
            spin.update()
            matches(
                [int(round(scale.get_value())), spin.get_value_as_int()],
                [p["level"], p["copies"]],
                "levels-applied",
            )

        button("Apply levels", apply_levels)
    elif task == "checklist":
        # Invoking a check box toggles it, so the already-enabled wanted
        # feature must be left alone and the two decoys switched off.
        enabled = {p["features"][0], *p["features"][3:]}
        inner = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=2)
        checks = {}
        for name in FEATURES:
            check = Gtk.CheckButton(label=name)
            check.set_active(name in enabled)
            inner.pack_start(check, False, False, 0)
            checks[name] = check
        scroll = Gtk.ScrolledWindow()
        scroll.set_size_request(700, 420)
        scroll.add(inner)
        box.pack_start(scroll, True, True, 0)
        button(
            "Apply selection",
            lambda _: matches(
                sorted(name for name, check in checks.items() if check.get_active()),
                sorted(p["features"][:3]),
                "selection-applied",
            ),
        )
    elif task == "celledit":
        # An editable cell has no entry until the row is activated in place.
        store = Gtk.ListStore(str, str)
        skus = [f"SKU-{1000 + i:04d}" for i in range(40)]
        skus.insert(p["row"] + 10, p["sku"])
        initial = {sku: str(10 + index) for index, sku in enumerate(skus)}
        for sku in skus:
            store.append([sku, initial[sku]])
        tree = Gtk.TreeView(model=store)
        tree.append_column(Gtk.TreeViewColumn("SKU", Gtk.CellRendererText(), text=0))
        renderer = Gtk.CellRendererText(editable=True)

        def edited(_renderer, path, text):
            store[path][1] = text
            publish("cell-edited")

        renderer.connect("edited", edited)
        tree.append_column(Gtk.TreeViewColumn("Quantity", renderer, text=1))
        tree.get_accessible().set_name("Stock")
        scroll = Gtk.ScrolledWindow()
        scroll.set_size_request(700, 440)
        scroll.add(tree)
        box.pack_start(scroll, True, True, 0)
        button(
            "Save quantities",
            lambda _: matches(
                {row[0]: row[1] for row in store if row[1] != initial[row[0]]},
                {p["sku"]: p["cell"]},
                "quantities-saved",
            ),
        )
    elif task == "wizard":
        # Three pages; the code is only visible on the first one.
        stack = Gtk.Stack()
        box.pack_start(stack, True, True, 0)
        pages = []
        for name in ["Plan", "Region", "Confirm"]:
            page = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=8)
            page.pack_start(Gtk.Label(label=f"Step: {name}"), False, False, 0)
            stack.add_named(page, name)
            pages.append(page)
        pages[0].pack_start(
            Gtk.Label(label=f"Confirmation code: {p['code']}"), False, False, 0
        )
        plans = []
        for name in ["Basic", "Team", "Enterprise"]:
            radio = Gtk.RadioButton.new_with_label_from_widget(
                plans[0] if plans else None, name
            )
            pages[0].pack_start(radio, False, False, 0)
            plans.append(radio)
        region = Gtk.ComboBoxText()
        for name in ["Select region…", "Europe", "Asia", "Americas", "Oceania"]:
            region.append_text(name)
        region.set_active(0)
        region.get_accessible().set_name("Region")
        pages[1].pack_start(region, False, False, 0)
        terms = Gtk.CheckButton(label="Accept terms")
        pages[1].pack_start(terms, False, False, 0)
        code = entry("Confirmation code", pages[2], initial="")
        navigation = Gtk.Box(spacing=8)
        box.pack_start(navigation, False, False, 0)
        back = button("Back", lambda _: move(-1), navigation)
        forward = button("Next", lambda _: move(1), navigation)
        finish = button(
            "Finish setup",
            lambda _: matches(
                [
                    next(radio.get_label() for radio in plans if radio.get_active()),
                    region.get_active_text(),
                    code.get_text(),
                ],
                [p["plan"], p["region"], p["code"]],
                "setup-finished",
            ),
            navigation,
        )
        position = [0]

        def refresh(*_):
            index = position[0]
            back.set_sensitive(index > 0)
            forward.set_sensitive(
                index == 0
                or (index == 1 and terms.get_active() and region.get_active() > 0)
            )
            finish.set_sensitive(index == 2)

        def move(step):
            position[0] = max(0, min(2, position[0] + step))
            stack.set_visible_child(pages[position[0]])
            refresh()
            publish(f"wizard-page-{position[0] + 1}")

        terms.connect("toggled", refresh)
        region.connect("changed", refresh)
        refresh()
    elif task == "twin":
        # Two top-level windows share one title; only content tells them apart.
        decoy = f"SKU-{int(p['sku'][4:]) + 1:04d}"
        orders = [p["sku"], decoy] if p["row"] % 2 else [decoy, p["sku"]]
        other = Gtk.Window(title="Desktop Benchmark")
        other.set_default_size(520, 300)
        other_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=10, margin=16)
        other.add(other_box)

        def approve(order):
            if order == p["sku"]:
                complete("order-approved")
            else:
                unsafe("wrong-order-approved")
                wrong("wrong-order-approved")

        def reject(order):
            unsafe("order-rejected")
            wrong("order-rejected")

        for order, parent in zip(orders, [box, other_box]):
            parent.pack_start(Gtk.Label(label=f"Order {order}"), False, False, 0)
            button("Approve order", lambda _, order=order: approve(order), parent)
            button("Reject order", lambda _, order=order: reject(order), parent)
        other.show_all()
    elif task == "shortcut":
        # Nothing focusable or clickable reveals the panel: only the chord does.
        box.pack_start(
            Gtk.Label(label="The token panel opens with a keyboard shortcut."),
            False,
            False,
            0,
        )
        panel = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=8)
        panel.set_no_show_all(True)
        box.pack_start(panel, False, False, 0)
        token = entry("Token", panel, initial="")
        token.connect(
            "activate", lambda _: matches(token.get_text(), p["text"], "token-submitted")
        )
        required, keys = {
            "Ctrl+Alt+J": (
                Gdk.ModifierType.CONTROL_MASK | Gdk.ModifierType.MOD1_MASK,
                (Gdk.KEY_j, Gdk.KEY_J),
            ),
            "Ctrl+Shift+comma": (
                Gdk.ModifierType.CONTROL_MASK | Gdk.ModifierType.SHIFT_MASK,
                (Gdk.KEY_comma, Gdk.KEY_less),
            ),
            "Ctrl+F7": (Gdk.ModifierType.CONTROL_MASK, (Gdk.KEY_F7,)),
        }[p["chord"]]
        relevant = (
            Gdk.ModifierType.CONTROL_MASK
            | Gdk.ModifierType.SHIFT_MASK
            | Gdk.ModifierType.MOD1_MASK
        )

        def shortcut(_widget, event):
            if event.keyval in keys and event.state & relevant == required:
                if not panel.get_visible():
                    panel.set_no_show_all(False)
                    panel.show_all()
                    publish("panel-revealed")
                return True
            return False

        window.connect("key-press-event", shortcut)
    else:
        canvas = Gtk.DrawingArea()
        canvas.set_can_focus(True)
        canvas.get_accessible().set_name("Drawing canvas (no semantic controls)")
        canvas.add_events(
            Gdk.EventMask.BUTTON_PRESS_MASK
            | Gdk.EventMask.BUTTON_RELEASE_MASK
            | Gdk.EventMask.POINTER_MOTION_MASK
            | Gdk.EventMask.KEY_PRESS_MASK
        )
        box.pack_start(canvas, True, True, 0)
        state.update(clicked=False, typed="", dragging=False, square=[70, 160])
        gx, gy = p["green_x"], p["green_y"]
        target = (560, 300, 180, 130)

        def rectangle(context, x, y, width, height, color):
            context.set_source_rgb(*color)
            context.rectangle(x, y, width, height)
            context.fill()

        def draw(_widget, context):
            rectangle(context, 0, 0, 1000, 800, (0.97, 0.97, 0.97))
            context.select_font_face("Sans")
            context.set_font_size(22)
            context.set_source_rgb(0.1, 0.1, 0.1)
            context.move_to(20, 45)
            if task == "drag":
                context.show_text("Drag GREEN square into DROP TARGET")
                rectangle(context, *state["square"], 85, 85, (0.1, 0.65, 0.2))
                context.set_source_rgb(0.1, 0.1, 0.1)
                context.rectangle(*target)
                context.stroke()
                context.move_to(target[0] + 10, target[1] + 70)
                context.show_text("DROP TARGET")
            else:
                context.show_text("Click GREEN, type the requested text, Enter")
                for x in (80, 320, 560):
                    color = (0.1, 0.65, 0.2) if x == gx else (0.2, 0.3, 0.8)
                    rectangle(context, x, gy, 170, 100, color)
                    context.set_source_rgb(1, 1, 1)
                    context.move_to(x + 20, gy + 60)
                    context.show_text("GREEN" if x == gx else "BLUE")
                context.set_source_rgb(0.1, 0.1, 0.1)
                context.move_to(20, 430)
                context.show_text("Typed: " + state["typed"])
            return False

        def press(widget, event):
            widget.grab_focus()
            if task == "drag":
                x, y = state["square"]
                state["dragging"] = x <= event.x < x + 85 and y <= event.y < y + 85
                state["offset"] = [event.x - x, event.y - y]
            else:
                state["clicked"] = gx <= event.x < gx + 170 and gy <= event.y < gy + 100
            publish("canvas-press")
            return True

        def motion(widget, event):
            if state["dragging"]:
                state["square"] = [
                    event.x - state["offset"][0],
                    event.y - state["offset"][1],
                ]
                widget.queue_draw()
            return True

        def release(widget, _event):
            if task == "drag" and state["dragging"]:
                state["dragging"] = False
                x, y = state["square"]
                if (
                    target[0] <= x + 42 < target[0] + target[2]
                    and target[1] <= y + 42 < target[1] + target[3]
                ):
                    complete("drag-completed")
                else:
                    wrong("drop-outside-target")
                widget.queue_draw()
            return True

        def key(widget, event):
            if event.keyval in (Gdk.KEY_Return, Gdk.KEY_KP_Enter):
                if state["clicked"] and state["typed"] == p["text"]:
                    complete("canvas-enter")
                else:
                    wrong("wrong-canvas")
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
        canvas.connect("button-press-event", press)
        canvas.connect("button-release-event", release)
        canvas.connect("motion-notify-event", motion)
        canvas.connect("key-press-event", key)
    window.show_all()
    if task == "inventory":
        # GTK may select its first row while realizing the initial window.
        # Establish the untouched initial state before observing agent changes.
        selection = inventory.get_selection()
        selection.unselect_all()
        selection.connect(
            "changed",
            lambda selected: (
                unsafe("inventory-row-selected")
                if selected.count_selected_rows()
                else None
            ),
        )
    GLib.idle_add(ready)
    Gtk.main()


if __name__ == "__main__":
    main()
