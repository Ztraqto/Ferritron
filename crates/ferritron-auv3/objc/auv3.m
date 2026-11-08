// AUv3 of a Ferritron plugin: the app extension's principal class. It
// registers the plugin's Audio Unit v2 (ferritron-au, the same code as the
// .component) inside the extension's process under a private description,
// and hands hosts an AUAudioUnitV2Bridge of it: parameters, state, MIDI,
// latency and tail behave exactly as in the AU v2. On macOS the view is the
// plugin's editor (web or GPU); on iOS hosts show the parameters.
//
// Written against Apple's documented API; never built or run on a Mac.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

#import <AudioToolbox/AudioToolbox.h>
#import <CoreAudioKit/CoreAudioKit.h>
#import <TargetConditionals.h>

// Rust (src/lib.rs): the v2 unit for a description.
extern void* ferritron_auv3_factory(const AudioComponentDescription* desc);
#if TARGET_OS_OSX
// Rust (ferritron-au): a container view with the editor of a v2 unit.
extern NSView* ferritron_au_view(AudioUnit unit, CGSize size);
#endif

// The private manufacturer the v2 unit is registered under in this process,
// so the bridge never finds the extension's own AUv3 component.
static const OSType kPrivateManufacturer = 'FrV2';

@interface FERRITRON_AUV3_CLASS : AUViewController <AUAudioUnitFactory>
@property (nonatomic, strong) AUAudioUnit* unit;
@end

@implementation FERRITRON_AUV3_CLASS

- (AUAudioUnit*)createAudioUnitWithComponentDescription:(AudioComponentDescription)desc error:(NSError**)error {
    AudioComponentDescription inner = desc;
    inner.componentManufacturer = kPrivateManufacturer;
    static dispatch_once_t once;
    dispatch_once(&once, ^{
        AudioComponentRegister(&inner, CFSTR("Ferritron v2"), 0x00010000,
                               (AudioComponentFactoryFunction)ferritron_auv3_factory);
    });
    AUAudioUnitV2Bridge* unit = [[AUAudioUnitV2Bridge alloc] initWithComponentDescription:inner error:error];
    self.unit = unit;
    dispatch_async(dispatch_get_main_queue(), ^{ [self attachEditor]; });
    return unit;
}

#if TARGET_OS_OSX
- (void)loadView {
    self.view = [[NSView alloc] initWithFrame:NSMakeRect(0, 0, 480, 320)];
    [self attachEditor];
}

- (void)attachEditor {
    if (!self.unit || !self.isViewLoaded || self.view.subviews.count > 0) return;
    AudioUnit au = ((AUAudioUnitV2Bridge*)self.unit).audioUnit;
    NSView* editor = ferritron_au_view(au, self.view.bounds.size);
    if (!editor) return;
    editor.frame = self.view.bounds;
    editor.autoresizingMask = NSViewWidthSizable | NSViewHeightSizable;
    [self.view addSubview:editor];
    self.preferredContentSize = editor.frame.size;
}
#else
- (void)loadView {
    self.view = [[UIView alloc] initWithFrame:CGRectMake(0, 0, 480, 320)];
}

- (void)attachEditor {
}
#endif

@end
