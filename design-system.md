# MICE Design System

The MICE design language is deeply rooted in native macOS aesthetics (AppKit), focusing on spatial overlays, glassmorphism, and system-native typography. The goal is to feel like a natural extension of macOS while retaining a distinctive MICE personality through vibrant, animated accents.

## Core Principles

1.  **Native Feel, Distinctive Accent**: Use system colors and typography, but employ specific vibrant gradients to distinguish MICE components from ordinary system windows.
2.  **Translucency & Glassmorphism**: Heavily rely on HUD materials (`.hudWindow` in AppKit) combined with subtle borders for depth and structure.
3.  **Visual Hierarchy through Contrast**: Use primary and secondary label colors strictly. Dimmed background elements help foreground text pop.

## Typography

MICE uses the system font (San Francisco) almost exclusively.

*   **Hero / Display**: 30pt Bold (Home Hero)
*   **Section Titles**: 20pt Semibold, 18pt Semibold
*   **Panel Titles (e.g., PromptPanel)**: 16pt Semibold
*   **Body Text & Inputs**: 14pt Regular
*   **Secondary Context & Details**: 13pt Regular, 12pt Medium
*   **Eyebrows & Micro-copy (Hints)**: 11pt Semibold, 11pt Medium
*   **Buttons**: 14pt Bold (Primary Glass), 13pt Semibold (Secondary), 11pt Semibold (Tertiary)

## Color Palette

### Surfaces & Backgrounds
*   **Panels**: System HUD Material (translucent dark/light adaptively).
*   **Elevated Cards (Input, Features)**: Black with `12%` to `26%` opacity for inset depth, or White with `5.5%` opacity for raised elements.
*   **Borders**: White with `8%` to `20%` opacity to define edges on translucent surfaces.

### Text
*   **Primary**: `.labelColor` (System adaptive text color)
*   **Secondary**: `.secondaryLabelColor` (System adaptive gray)
*   **On-Accent**: White (for text on top of vibrant gradients)

### MICE Accent Gradients
MICE uses two primary gradients for highlights, glows, and primary actions.

1.  **Primary Glass Button Gradient**:
    `System Pink` → `System Orange` → `System Teal` → `System Blue` → `System Purple`
2.  **MICE Accent / Highlight Gradient (miceAccentColors)**:
    `rgba(97, 214, 255, 1)` (Cyan) → `rgba(125, 140, 255, 1)` (Light Blue) → `rgba(207, 120, 255, 1)` (Purple) → `rgba(255, 143, 189, 1)` (Pink) → `rgba(255, 212, 122, 1)` (Orange)

## Component Styling

### Panels (e.g., PromptPanel)
*   **Corner Radius**: 18pt or 20pt.
*   **Padding**: Generous outer padding (typically 24pt).
*   **Background**: `.hudWindow` visual effect view.

### Cards & Inputs
*   **Corner Radius**: 12pt or 13pt.
*   **Background**: Deep translucent black (e.g., `black` at `26%` for `PromptPanel` input card).
*   **Border**: 1px translucent white border (e.g., `white` at `20%`).
*   **Text Field**: Borderless, clear background, no focus ring.

### Buttons
*   **Primary Glass Button**: Distinctive vibrant gradient background (see above) with a 13pt corner radius. The button content (text) is inset by 1pt on all sides to create a subtle border effect, with White text (`14pt Bold`).
*   **Secondary Buttons**: Standard rounded bezel buttons (13pt Semibold).

## Layout & Spacing
*   **PromptPanel Structure**:
    *   Title (16pt Semibold)
    *   *Spacing: 16pt* (if context exists)
    *   Context (13pt Regular)
    *   *Spacing: 16pt*
    *   Input Card (Height: 40pt)
    *   *Spacing: 16pt*
    *   Hint (11pt Medium, Centered)
