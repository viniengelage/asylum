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

function serialize(node) {
  if (node.visible === false) return null
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
  }
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
