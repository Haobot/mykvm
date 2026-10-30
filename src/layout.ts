import type { Device, LayoutState, Screen } from './types'

const SNAP_TOLERANCE = 80

export interface FlattenedScreen extends Screen {
  deviceName: string
  deviceColor: string
  platform: Device['platform']
  online: boolean
  inputReady: boolean
  upgrading?: boolean
  role: Device['role']
}

export interface LayoutBounds {
  minX: number
  minY: number
  maxX: number
  maxY: number
  width: number
  height: number
}

export interface ScreenPosition {
  x: number
  y: number
}

/**
 * Pushes a screen the minimum distance needed to stop overlapping any other
 * screen. Separation runs along the axis with the smaller overlap, so the two
 * rectangles end up exactly touching — exactly the edge geometry the Rust side
 * needs to treat the boundary as crossable.
 */
export function separateOverlappingScreens(
  layout: LayoutState,
  screenId: string,
): LayoutState {
  const movingScreen = getScreenById(layout, screenId)
  if (!movingScreen) {
    return layout
  }

  const others = flattenScreens(layout).filter((screen) => screen.id !== screenId)
  let position: ScreenPosition = { x: movingScreen.x, y: movingScreen.y }

  // Each pass separates from the largest overlapping screen; at most one pass
  // per other screen is enough to settle a chain of overlaps.
  for (let pass = 0; pass < others.length; pass += 1) {
    let target: FlattenedScreen | null = null
    let targetArea = 0
    for (const screen of others) {
      const area = screenOverlapArea({ ...movingScreen, ...position }, screen)
      if (area > targetArea) {
        target = screen
        targetArea = area
      }
    }
    if (!target) {
      break
    }

    position = separateFromScreen({ ...movingScreen, ...position }, target)
  }

  return moveScreen(layout, screenId, position)
}

function separateFromScreen(moving: Screen, other: Screen): ScreenPosition {
  const overlapX =
    Math.min(moving.x + moving.width, other.x + other.width) - Math.max(moving.x, other.x)
  const overlapY =
    Math.min(moving.y + moving.height, other.y + other.height) - Math.max(moving.y, other.y)

  if (overlapX <= 0 || overlapY <= 0) {
    return { x: moving.x, y: moving.y }
  }

  const movingCenterX = moving.x + moving.width / 2
  const movingCenterY = moving.y + moving.height / 2
  const otherCenterX = other.x + other.width / 2
  const otherCenterY = other.y + other.height / 2

  if (overlapX <= overlapY) {
    return {
      x: movingCenterX <= otherCenterX ? other.x - moving.width : other.x + other.width,
      y: moving.y,
    }
  }

  return {
    x: moving.x,
    y: movingCenterY <= otherCenterY ? other.y - moving.height : other.y + other.height,
  }
}

export function flattenScreens(layout: LayoutState): FlattenedScreen[] {
  return layout.devices.flatMap((device) =>
    device.screens.map((screen) => ({
      ...screen,
      deviceName: device.name,
      deviceColor: device.color,
      platform: device.platform,
      online: device.online,
      inputReady: device.inputReady,
      upgrading: device.upgrading,
      role: device.role,
    })),
  )
}

export function getLayoutBounds(screens: Screen[]): LayoutBounds {
  const minX = Math.min(...screens.map((screen) => screen.x))
  const minY = Math.min(...screens.map((screen) => screen.y))
  const maxX = Math.max(...screens.map((screen) => screen.x + screen.width))
  const maxY = Math.max(...screens.map((screen) => screen.y + screen.height))

  return {
    minX,
    minY,
    maxX,
    maxY,
    width: Math.max(maxX - minX, 1),
    height: Math.max(maxY - minY, 1),
  }
}

function screenOverlapArea(a: Screen, b: Screen) {
  const width = Math.min(a.x + a.width, b.x + b.width) - Math.max(a.x, b.x)
  const height = Math.min(a.y + a.height, b.y + b.height) - Math.max(a.y, b.y)
  return width > 0 && height > 0 ? width * height : 0
}

export function moveScreen(
  layout: LayoutState,
  screenId: string,
  nextPosition: ScreenPosition,
): LayoutState {
  return {
    ...layout,
    devices: layout.devices.map((device) => ({
      ...device,
      screens: device.screens.map((screen) =>
        screen.id === screenId ? { ...screen, ...nextPosition } : screen,
      ),
    })),
  }
}

export function snapScreenPosition(
  layout: LayoutState,
  screenId: string,
  nextPosition: ScreenPosition,
): ScreenPosition {
  const movingScreen = getScreenById(layout, screenId)
  if (!movingScreen) {
    return nextPosition
  }

  const screens = flattenScreens(layout).filter((screen) => screen.id !== screenId)

  return {
    x: snapAxis(
      nextPosition.x,
      screens.flatMap((screen) => [
        screen.x,
        screen.x + screen.width,
        screen.x - movingScreen.width,
        screen.x + screen.width - movingScreen.width,
      ]),
    ),
    y: snapAxis(
      nextPosition.y,
      screens.flatMap((screen) => [
        screen.y,
        screen.y + screen.height,
        screen.y - movingScreen.height,
        screen.y + screen.height - movingScreen.height,
      ]),
    ),
  }
}

function snapAxis(value: number, candidates: number[]) {
  let closest = value
  let closestDistance = SNAP_TOLERANCE + 1

  for (const candidate of candidates) {
    const distance = Math.abs(value - candidate)
    if (distance < closestDistance) {
      closest = candidate
      closestDistance = distance
    }
  }

  return closestDistance <= SNAP_TOLERANCE ? closest : value
}

export function getScreenById(layout: LayoutState, screenId: string) {
  return flattenScreens(layout).find((screen) => screen.id === screenId)
}
