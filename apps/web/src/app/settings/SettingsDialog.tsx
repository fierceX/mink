/**
 * Settings dialog: appearance (the three preference cubes), the content font
 * size stepper and the process-display preference. Chrome comes from the copied
 * Modal, the cubes and the stepper keep the source's own class contracts
 * (ui-theme AppearanceRow/FontSizeRow stylesheets).
 */

import clsx from 'clsx'
import {
  IconChevronDownOutlineRegular,
  IconChevronUpOutlineRegular,
  IconDarkOutlineMedium,
  IconFollowsystemOutlineMedium,
  IconLightOutlineMedium,
  Modal,
  SegmentedControl,
} from '../../ui/primitives/index.ts'
import {
  FONT_SIZE_MAX,
  FONT_SIZE_MIN,
  type Preferences,
  type ProcessDisplay,
  type ThemePreference,
} from './preferences.ts'
import appearanceCss from './AppearanceRow.module.css'
import fontSizeCss from './FontSizeRow.module.css'
import css from './SettingsDialog.module.css'

/** Cube order and icons: Light, Dark, System. */
const CUBES: readonly { id: ThemePreference; label: string; Icon: typeof IconLightOutlineMedium }[] = [
  { id: 'light', label: '浅色', Icon: IconLightOutlineMedium },
  { id: 'dark', label: '深色', Icon: IconDarkOutlineMedium },
  { id: 'system', label: '跟随系统', Icon: IconFollowsystemOutlineMedium },
]

/** Process-display options, in the order the workbench reads them. */
const PROCESS_OPTIONS: readonly { value: ProcessDisplay; label: string }[] = [
  { value: 'concise', label: '简洁' },
  { value: 'standard', label: '标准' },
  { value: 'detailed', label: '详细' },
]

/**
 * Render the settings dialog.
 * @param props.preferences - current preferences.
 * @param props.onChange - apply a preference patch.
 * @param props.onClose - dismiss the dialog.
 * @returns the dialog element tree.
 */
export function SettingsDialog({ preferences, onChange, onClose }: {
  preferences: Preferences
  onChange: (patch: Partial<Preferences>) => void
  onClose: () => void
}) {
  return (
    <Modal
      open
      title="设置"
      closeLabel="关闭设置"
      className={css.dialogSize}
      contentClassName={css.dialogContent}
      onClose={onClose}
    >
      <div className={css.sections}>
        <section className={appearanceCss.group} aria-label="外观">
          <div className={appearanceCss.title}>外观</div>
          <div className={appearanceCss.cubeRow}>
            {CUBES.map(({ id, label, Icon }) => (
              <button
                key={id}
                type="button"
                className={clsx(appearanceCss.themeCube, preferences.theme === id && appearanceCss.selected)}
                aria-pressed={preferences.theme === id}
                onClick={() => {
                  onChange({ theme: id })
                }}
              >
                <Icon />
                {label}
              </button>
            ))}
          </div>
        </section>

        <section className={fontSizeCss.row} aria-label="正文字号">
          <div className={fontSizeCss.rowText}>
            <div className={fontSizeCss.title}>正文字号</div>
            <div className={fontSizeCss.desc}>会话正文与代码块的基准字号。</div>
          </div>
          <div className={fontSizeCss.control}>
            <div className={fontSizeCss.stepper}>
              <span className={fontSizeCss.value}>{preferences.fontSize}</span>
              <span className={fontSizeCss.arrows}>
                <button
                  type="button"
                  className={fontSizeCss.arrow}
                  aria-label="增大字号"
                  disabled={preferences.fontSize >= FONT_SIZE_MAX}
                  onClick={() => {
                    onChange({ fontSize: preferences.fontSize + 1 })
                  }}
                >
                  <IconChevronUpOutlineRegular size={9} />
                </button>
                <button
                  type="button"
                  className={fontSizeCss.arrow}
                  aria-label="减小字号"
                  disabled={preferences.fontSize <= FONT_SIZE_MIN}
                  onClick={() => {
                    onChange({ fontSize: preferences.fontSize - 1 })
                  }}
                >
                  <IconChevronDownOutlineRegular size={9} />
                </button>
              </span>
            </div>
            <span className={fontSizeCss.unit}>px</span>
          </div>
        </section>

        <section className={css.section} aria-label="过程展示">
          <div className={css.sectionTitle}>过程展示</div>
          <p className={css.sectionDesc}>
            简洁只保留结论；标准折叠过程组；详细默认展开每个工具调用。
          </p>
          <SegmentedControl
            id="mink-process-display"
            label="过程展示"
            options={PROCESS_OPTIONS}
            value={preferences.process}
            onChange={(value) => {
              onChange({ process: value })
            }}
          />
        </section>
      </div>
    </Modal>
  )
}
