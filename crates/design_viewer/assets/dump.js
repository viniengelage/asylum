function paints(list) {
  if (!Array.isArray(list)) return []
  return list.filter(paint => paint.visible !== false).map(paint => {
    const entry = { type: paint.type, opacity: paint.opacity ?? 1, blendMode: paint.blendMode }
    if (paint.type === 'SOLID') entry.color = paint.color
    if (paint.type && paint.type.startsWith('GRADIENT')) {
      entry.stops = paint.gradientStops
      entry.transform = paint.gradientTransform
    }
    return entry
  })
}

function boxOf(node) {
  return node.absoluteBoundingBox || { x: node.x, y: node.y, width: node.width, height: node.height }
}

function variableName(alias) {
  try {
    return alias && alias.id ? figma.getVariableById(alias.id)?.name : undefined
  } catch {
    return undefined
  }
}

// A binding is one alias or, for paints and text ranges, a list of them.
function boundName(binding) {
  const aliases = Array.isArray(binding) ? binding : [binding]
  for (const alias of aliases) {
    const name = variableName(alias)
    if (name) return name
  }
  return undefined
}

function paintToken(node, key) {
  const bound = node.boundVariables || {}
  const direct = boundName(bound[key])
  if (direct) return direct
  if (!Array.isArray(node[key])) return undefined
  for (const paint of node[key]) {
    const name = paint && paint.boundVariables ? boundName(paint.boundVariables.color) : undefined
    if (name) return name
  }
  return undefined
}

function tokens(node) {
  const result = {}
  try {
    const bound = node.boundVariables || {}
    const add = (property, name) => {
      if (name) result[property] = name
    }
    const first = keys => keys.map(key => boundName(bound[key])).find(Boolean)
    add('fills', paintToken(node, 'fills'))
    add('strokes', paintToken(node, 'strokes'))
    add('radius', first(['cornerRadius', 'topLeftRadius', 'topRightRadius', 'bottomRightRadius', 'bottomLeftRadius']))
    add('itemSpacing', first(['itemSpacing']))
    add('padding', first(['paddingTop', 'paddingRight', 'paddingBottom', 'paddingLeft']))
    add('effects', first(['effects']))
    add('fontSize', first(['fontSize']))
    add('fontFamily', first(['fontFamily']))
    add('fontWeight', first(['fontWeight', 'fontStyle']))
  } catch {}
  return Object.keys(result).length ? result : undefined
}

function componentName(node) {
  try {
    const main = node.mainComponent
    if (!main) return undefined
    return main.parent && main.parent.type === 'COMPONENT_SET' ? `${main.parent.name} / ${main.name}` : main.name
  } catch {
    return undefined
  }
}

function serialize(node) {
  const entry = {
    id: node.id,
    type: node.type,
    name: node.name,
    box: boxOf(node),
    opacity: node.opacity ?? 1,
    blendMode: node.blendMode,
    fills: paints(node.fills),
    strokes: paints(node.strokes),
    strokeWeight: typeof node.strokeWeight === 'number' ? node.strokeWeight : undefined,
    radius: typeof node.cornerRadius === 'number' ? node.cornerRadius : [node.topLeftRadius, node.topRightRadius, node.bottomRightRadius, node.bottomLeftRadius],
    clips: node.clipsContent === true,
    effects: (node.effects || []).filter(effect => effect.visible !== false).map(effect => ({ type: effect.type, color: effect.color, offset: effect.offset, radius: effect.radius, spread: effect.spread })),
    visible: node.visible === false ? false : undefined,
    layoutMode: node.layoutMode,
    itemSpacing: typeof node.itemSpacing === 'number' ? node.itemSpacing : undefined,
    paddingTop: node.paddingTop,
    paddingRight: node.paddingRight,
    paddingBottom: node.paddingBottom,
    paddingLeft: node.paddingLeft,
    primaryAxisAlignItems: node.primaryAxisAlignItems,
    counterAxisAlignItems: node.counterAxisAlignItems,
    layoutSizingHorizontal: node.layoutSizingHorizontal,
    layoutSizingVertical: node.layoutSizingVertical,
    tokens: tokens(node),
  }
  if (node.type === 'INSTANCE') entry.component = componentName(node)
  if (node.type === 'TEXT') {
    Object.assign(entry, {
      characters: node.characters,
      fontName: node.fontName,
      fontSize: typeof node.fontSize === 'number' ? node.fontSize : undefined,
      lineHeight: node.lineHeight,
      letterSpacing: node.letterSpacing,
      align: node.textAlignHorizontal,
      autoResize: node.textAutoResize,
    })
  }
  if (node.vectorPaths && node.vectorPaths.length) {
    entry.paths = node.vectorPaths.map(path => path.data)
    entry.size = { width: node.width, height: node.height }
  }
  if ('children' in node && node.type !== 'TEXT') {
    entry.children = node.children.map(serialize).filter(Boolean)
  }
  return entry
}

function background(page) {
  const paint = (page.backgrounds || []).find(candidate => candidate.visible !== false && candidate.type === 'SOLID')
  return paint ? { ...paint.color, a: (paint.color.a ?? 1) * (paint.opacity ?? 1) } : undefined
}

function screens(page) {
  const list = []
  const count = node => 1 + ('children' in node && node.type !== 'TEXT' ? node.children.reduce((total, child) => total + count(child), 0) : 0)
  const add = node => list.push({ id: node.id, name: node.name, box: boxOf(node), nodeCount: count(node) })
  for (const node of page.children) {
    if (node.visible === false) continue
    if (node.type === 'FRAME') add(node)
    if (node.type === 'FRAME' || node.type === 'SECTION') {
      for (const child of node.children) {
        if (child.visible !== false && child.type === 'FRAME') add(child)
      }
    }
  }
  return list
}

return {
  pages: figma.root.children.map(page => ({
    id: page.id,
    name: page.name,
    background: background(page),
    children: page.children.map(serialize).filter(Boolean),
    screens: screens(page),
  })),
}
