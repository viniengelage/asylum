const MAX_DEPTH = 6
const MAX_LINES = 400
const MAX_TEXT = 120

const node = figma.getNodeById(NODE_ID)
if (!node) return null

const lines = []
const size = target => `${Math.round(target.width)}×${Math.round(target.height)}`

function layout(target) {
  if (!target.layoutMode || target.layoutMode === 'NONE') return ''
  const parts = [target.layoutMode === 'HORIZONTAL' ? 'row' : 'column']
  if (target.itemSpacing) parts.push(`gap ${target.itemSpacing}`)
  const padding = [target.paddingTop, target.paddingRight, target.paddingBottom, target.paddingLeft].map(value => value || 0)
  if (padding.some(value => value)) parts.push(`padding ${padding.join(' ')}`)
  return ` · ${parts.join(' ')}`
}

function walk(target, depth) {
  if (target.visible === false) return
  if (lines.length >= MAX_LINES) return
  const indent = '  '.repeat(depth)
  let line = `${indent}${target.type} "${target.name}" · ${size(target)}${layout(target)}`
  if (target.type === 'TEXT' && target.characters) {
    line += ` · "${target.characters.replace(/\s+/g, ' ').slice(0, MAX_TEXT)}"`
  }
  lines.push(line)
  // An instance's layers come from its component, so its name says enough.
  if (target.type === 'INSTANCE' || target.type === 'TEXT') return
  if (!('children' in target) || target.children.length === 0) return
  if (depth >= MAX_DEPTH) {
    lines.push(`${indent}  … ${target.children.length} children`)
    return
  }
  for (const child of target.children) walk(child, depth + 1)
}

walk(node, 0)
if (lines.length >= MAX_LINES) lines.push('…')
return lines.join('\n')
