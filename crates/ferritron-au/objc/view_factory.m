// The Cocoa view factory of Ferritron Audio Units. Hosts read its class name
// from kAudioUnitProperty_CocoaUI, find it in the bundle and ask it for the
// editor view; the view itself is made in Rust (ferritron_au_view).
//
// SPDX-License-Identifier: MIT OR Apache-2.0

#import <AppKit/AppKit.h>
#import <AudioToolbox/AudioToolbox.h>
#import <AudioToolbox/AUCocoaUIView.h>

// Rust (src/view.rs): an autoreleased view of `size` showing the editor of `unit`.
extern NSView* ferritron_au_view(AudioUnit unit, NSSize size);

@interface FERRITRON_AU_VIEW_CLASS : NSObject <AUCocoaUIBase>
@end

@implementation FERRITRON_AU_VIEW_CLASS

- (unsigned)interfaceVersion {
    return 0;
}

- (NSString*)description {
    return @"Ferritron editor";
}

- (NSView*)uiViewForAudioUnit:(AudioUnit)unit withSize:(NSSize)size {
    return ferritron_au_view(unit, size);
}

@end
