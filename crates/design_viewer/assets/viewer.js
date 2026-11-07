// Pan, zoom and screen focus for a page rendered by the design viewer. State goes back to
// Asylum through the URL hash (`screen`, `zoom`, `x`, `y`), which is also where a reload picks
// it up again, so a page swapped in after an edit keeps its zoom and scroll.
(() => {
  const stage = document.getElementById('asylum-design-stage')
  if (!stage) return
  const screensElement = document.getElementById('asylum-design-screens')
  const screens = screensElement ? JSON.parse(screensElement.textContent || '[]') : []
  const root = document.documentElement

  const MIN_ZOOM = 0.02
  const MAX_ZOOM = 16
  const PADDING = 48
  const DRAG_THRESHOLD = 3
  const STEP = 1.25

  const params = new URLSearchParams(location.hash.slice(1))
  let accent = params.get('accent') || '#7c6cf2'
  let zoom = 1
  let x = 0
  let y = 0
  let focused = null

  const style = document.createElement('style')
  style.textContent =
    'html,body{height:100%;overflow:hidden}' +
    'body{user-select:none;-webkit-user-select:none;cursor:default}' +
    '#asylum-design-stage{position:absolute!important;left:0;top:0;transform-origin:0 0}' +
    '#asylum-design-outline{position:fixed;pointer-events:none;box-sizing:border-box;border-radius:3px;display:none}'
  document.head.appendChild(style)
  const outline = document.createElement('div')
  outline.id = 'asylum-design-outline'
  document.body.appendChild(outline)

  const clamp = value => Math.min(MAX_ZOOM, Math.max(MIN_ZOOM, value))

  // A reload after the .fig changes reads these back from the hash.
  function rememberParam(name, value) {
    const next = new URLSearchParams(location.hash.slice(1))
    if (value) next.set(name, value)
    else next.delete(name)
    history.replaceState(null, '', '#' + next.toString())
  }

  function setBackground(value) {
    if (!value || value === 'document') root.style.removeProperty('--design-viewer-bg')
    else root.style.setProperty('--design-viewer-bg', value)
    rememberParam('bg', value)
  }

  function setAccent(value) {
    accent = value
    rememberParam('accent', value)
    apply()
  }

  let reportTimer = null
  function report(immediate) {
    clearTimeout(reportTimer)
    const write = () => {
      const next = new URLSearchParams(location.hash.slice(1))
      if (focused) next.set('screen', focused.id)
      else next.delete('screen')
      next.set('zoom', String(Math.round(zoom * 1000) / 10))
      next.set('x', String(Math.round(x)))
      next.set('y', String(Math.round(y)))
      history.replaceState(null, '', '#' + next.toString())
    }
    // Panning reports on every wheel tick otherwise, and each report repaints Asylum's toolbar.
    if (immediate) write()
    else reportTimer = setTimeout(write, 150)
  }

  function apply() {
    stage.style.transform = `translate(${x}px, ${y}px) scale(${zoom})`
    if (!focused) {
      outline.style.display = 'none'
      return
    }
    outline.style.display = 'block'
    outline.style.border = `2px solid ${accent}`
    outline.style.left = `${x + focused.x * zoom - 2}px`
    outline.style.top = `${y + focused.y * zoom - 2}px`
    outline.style.width = `${focused.width * zoom + 4}px`
    outline.style.height = `${focused.height * zoom + 4}px`
  }

  function fitRect(rect) {
    const width = Math.max(1, window.innerWidth - PADDING * 2)
    const height = Math.max(1, window.innerHeight - PADDING * 2)
    zoom = clamp(Math.min(width / Math.max(1, rect.width), height / Math.max(1, rect.height)))
    x = (window.innerWidth - rect.width * zoom) / 2 - rect.x * zoom
    y = (window.innerHeight - rect.height * zoom) / 2 - rect.y * zoom
  }

  function fitPage() {
    fitRect({ x: 0, y: 0, width: stage.offsetWidth, height: stage.offsetHeight })
  }

  function fit() {
    if (focused) fitRect(focused)
    else fitPage()
    apply()
    report(true)
  }

  function focus(id) {
    const screen = screens.find(candidate => candidate.id === id)
    if (!screen) return
    focused = screen
    fit()
  }

  function step(delta) {
    if (!screens.length) return
    const index = focused ? screens.indexOf(focused) : -1
    const next = index < 0 ? (delta > 0 ? 0 : screens.length - 1) : index + delta
    if (next < 0 || next >= screens.length) return
    focus(screens[next].id)
  }

  function exit() {
    focused = null
    fit()
  }

  function zoomBy(factor, centerX, centerY) {
    const pivotX = centerX ?? window.innerWidth / 2
    const pivotY = centerY ?? window.innerHeight / 2
    const next = clamp(zoom * factor)
    x = pivotX - (pivotX - x) * (next / zoom)
    y = pivotY - (pivotY - y) * (next / zoom)
    zoom = next
    apply()
    report(false)
  }

  function screenAt(clientX, clientY) {
    const pageX = (clientX - x) / zoom
    const pageY = (clientY - y) / zoom
    let best = null
    for (const screen of screens) {
      const inside =
        pageX >= screen.x &&
        pageY >= screen.y &&
        pageX <= screen.x + screen.width &&
        pageY <= screen.y + screen.height
      // Screens nest inside sections, so the innermost (smallest) one is the one meant.
      if (inside && (!best || screen.width * screen.height < best.width * best.height)) best = screen
    }
    return best
  }

  let pointer = null
  window.addEventListener('pointerdown', event => {
    if (event.button !== 0 && event.button !== 1) return
    pointer = { startX: event.clientX, startY: event.clientY, lastX: event.clientX, lastY: event.clientY, dragging: false }
    event.preventDefault()
  })
  window.addEventListener('pointermove', event => {
    if (!pointer) return
    if (!pointer.dragging) {
      const distance = Math.hypot(event.clientX - pointer.startX, event.clientY - pointer.startY)
      if (distance < DRAG_THRESHOLD) return
      pointer.dragging = true
      document.body.style.cursor = 'grabbing'
    }
    x += event.clientX - pointer.lastX
    y += event.clientY - pointer.lastY
    pointer.lastX = event.clientX
    pointer.lastY = event.clientY
    apply()
    report(false)
  })
  window.addEventListener('pointerup', event => {
    if (!pointer) return
    const wasDrag = pointer.dragging
    pointer = null
    document.body.style.cursor = ''
    if (wasDrag || event.button !== 0) return
    const screen = screenAt(event.clientX, event.clientY)
    if (screen) focus(screen.id)
  })

  window.addEventListener(
    'wheel',
    event => {
      event.preventDefault()
      const lines = event.deltaMode === 1 ? 16 : 1
      if (event.ctrlKey || event.metaKey) {
        zoomBy(Math.exp(-event.deltaY * lines * 0.01), event.clientX, event.clientY)
        return
      }
      x -= event.deltaX * lines
      y -= event.deltaY * lines
      apply()
      report(false)
    },
    { passive: false },
  )

  window.addEventListener('keydown', event => {
    const command = event.metaKey || event.ctrlKey
    if (event.key === 'ArrowLeft') step(-1)
    else if (event.key === 'ArrowRight') step(1)
    else if (event.key === 'Escape') exit()
    else if (command && event.key === '0') fit()
    else if (command && (event.key === '=' || event.key === '+')) zoomBy(STEP)
    else if (command && event.key === '-') zoomBy(1 / STEP)
    else return
    event.preventDefault()
  })

  window.addEventListener('resize', () => {
    if (focused) fit()
  })

  window.asylumDesign = {
    zoomIn: () => zoomBy(STEP),
    zoomOut: () => zoomBy(1 / STEP),
    zoomBy,
    fit,
    focus,
    next: () => step(1),
    prev: () => step(-1),
    exit,
    setBackground,
    setAccent,
  }

  setBackground(params.get('bg'))
  const initialScreen = screens.find(candidate => candidate.id === params.get('screen')) || null
  const restoredZoom = Number(params.get('zoom'))
  if (params.has('x') && params.has('y') && restoredZoom > 0) {
    focused = initialScreen
    zoom = clamp(restoredZoom / 100)
    x = Number(params.get('x')) || 0
    y = Number(params.get('y')) || 0
    apply()
  } else if (initialScreen) {
    focus(initialScreen.id)
  } else {
    fit()
  }
})()
