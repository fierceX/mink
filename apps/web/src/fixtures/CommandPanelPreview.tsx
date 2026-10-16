// Design-review sample of the command surface: the same copied shell the
// composer opens, with a stand-in context so the card renders on the page
// without a session.
import { useMemo } from "react";
import { PopupSelectController } from "../ui/commands/popup.ts";
import { PopupSelectView } from "../ui/commands/PopupSelectView.tsx";
import { COMMAND_LOCALE, commandText } from "../app/commands/locale.ts";
import { commandPopupOptions } from "../app/conversation/useCommandPopup.ts";
import type { CommandContext } from "../app/commands/contract.ts";

/** The gallery's card: opened once on mount, closed by its own Escape. */
export function CommandPanelPreview() {
  const controller = useMemo(() => new PopupSelectController<CommandContext>({
    consume: () => true,
    focusComposer: () => {},
  }), []);

  useMemo(() => {
    const context: CommandContext = {
      write: () => {},
      run: () => {},
      pickFiles: () => {},
      canAttach: true,
    };
    controller.open("mink", {
      searchMode: "substring",
      searchLabels: () => ({ placeholder: "搜索命令…", empty: "无可用命令", noResults: "没有匹配的命令" }),
      options: async () => commandPopupOptions(true, true),
      onSelect: () => {},
    }, context, { via: "menu", span: { start: 0, end: 0, revision: 0 } });
  }, [controller]);

  return (
    <PopupSelectView
      popup={controller}
      t={(key, params) => commandText(COMMAND_LOCALE, key, params)}
    />
  );
}
