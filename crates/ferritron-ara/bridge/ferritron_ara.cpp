// ARA 2 for Ferritron plugins, on top of the ARA SDK's ARA_Library (which the
// developer provides through ARA_SDK_DIR; Ferritron does not ship it).
//
// The document controller keeps every audio source the host enables in
// memory, and each playback renderer renders the playback regions assigned
// to it from there. What a region sounds like is up to the plugin: for each
// part of a region inside the block, the Rust side's callback receives the
// source's samples (random access) and fills the output.

#include "ARA_Library/PlugIn/ARAPlug.h"

#include <algorithm>
#include <atomic>
#include <cmath>
#include <cstring>
#include <memory>
#include <string>
#include <thread>
#include <vector>

namespace {

/* A note found by the plugin's analysis (ARAContentNote without the ARA types). */
struct NoteData {
	float frequency;
	int32_t pitch;
	float volume;
	double start, attack, duration, signal;
};
using PushNote = void (*)(void* ctx, const NoteData* note);
using AnalyzeCallback = void (*)(const float* const* source, uint32_t channels, int64_t samples, double rate, void* ctx, PushNote push);

struct Config {
	std::string factoryId, name, vendor, url, version, archiveId;
	bool stretch = false;
	AnalyzeCallback analyze = nullptr;
};
Config g_config;

constexpr ARA::ARAChannelCount kMaxChannels = 8;

/* Renders one part of a region: `frames` samples starting at `source_start`
   in the source, into `out` (silent on entry).
   `ratio` is 0 for a region played as it is, else the source samples per output
   sample of a stretched one; `region_offset` is the position of the first
   output sample inside the region (in playback samples). */
using RegionCallback = void (*)(void* ctx, const float* const* source, uint32_t source_channels, int64_t source_samples,
                                double source_rate, double source_start, double ratio, int64_t region_offset,
                                float* const* out, uint32_t out_channels, uint32_t frames);

class DocumentController;

class AudioSource final : public ARA::PlugIn::AudioSource {
public:
	using ARA::PlugIn::AudioSource::AudioSource;

	~AudioSource() override { joinAnalysis(); }

	void updateCache() {
		joinAnalysis(); // the analysis reads the old samples
		_notesState = 0;
		_notes.clear();
		const auto channels = static_cast<size_t>(getChannelCount());
		const auto samples = static_cast<size_t>(getSampleCount());
		auto cache = std::make_shared<std::vector<float>>(channels * samples);
		std::vector<void*> pointers(channels);
		for (size_t c = 0; c < channels; ++c)
			pointers[c] = cache->data() + c * samples;
		// The host clears the buffers itself if reading fails (and tells the user).
		ARA::PlugIn::HostAudioReader reader{this};
		if (samples > 0)
			reader.readAudioSamples(0, static_cast<ARA::ARASampleCount>(samples), pointers.data());
		_cache = std::move(cache);
		_cachedChannels = channels;
		_cachedSamples = samples;
	}

	void clearCache() {
		joinAnalysis();
		_notesState = 0;
		_notes.clear();
		_cache.reset();
		_cachedChannels = _cachedSamples = 0;
	}

	bool cached() const { return _cachedSamples > 0 && _cachedChannels > 0; }
	size_t cachedChannels() const { return _cachedChannels; }
	size_t cachedSamples() const { return _cachedSamples; }
	const float* channel(size_t c) const { return _cache->data() + c * _cachedSamples; }

	// --- note analysis (main thread, except for the analysis thread itself) ---

	// The host asked for notes: they are found as soon as the samples are readable.
	void wantNotes() { _wantNotes = true; }
	bool notesWanted() const { return _wantNotes; }

	// 0 = nothing yet, 1 = running, 2 = done (host not told), 3 = done (host told)
	int notesState() const { return _notesState.load(std::memory_order_acquire); }
	void notesTold() { _notesState.store(3, std::memory_order_release); }
	const std::vector<NoteData>& notes() const { return _notes; }

	// Starts the analysis on a background thread; `done` runs on that thread at the end.
	template <typename Done>
	void startAnalysis(Done done) {
		if (!g_config.analyze || !cached() || _notesState != 0)
			return;
		joinAnalysis();
		_notesState = 1;
		auto cache = _cache;
		const auto channels = _cachedChannels, samples = _cachedSamples;
		const auto rate = getSampleRate();
		_thread = std::thread([this, cache, channels, samples, rate, done] {
			std::vector<const float*> pointers(channels);
			for (size_t c = 0; c < channels; ++c)
				pointers[c] = cache->data() + c * samples;
			std::vector<NoteData> found;
			g_config.analyze(pointers.data(), static_cast<uint32_t>(channels), static_cast<int64_t>(samples), rate, &found,
			                 [](void* ctx, const NoteData* note) { static_cast<std::vector<NoteData>*>(ctx)->push_back(*note); });
			std::sort(found.begin(), found.end(), [](const NoteData& a, const NoteData& b) { return a.start < b.start; });
			_notes = std::move(found);
			_notesState.store(2, std::memory_order_release);
			done();
		});
	}

	void joinAnalysis() {
		if (_thread.joinable())
			_thread.join();
	}

private:
	std::shared_ptr<std::vector<float>> _cache;
	size_t _cachedChannels = 0;
	size_t _cachedSamples = 0;
	bool _wantNotes = false;
	std::atomic<int> _notesState{0};
	std::vector<NoteData> _notes;
	std::thread _thread;
};

class PlaybackRenderer final : public ARA::PlugIn::PlaybackRenderer {
public:
	using ARA::PlugIn::PlaybackRenderer::PlaybackRenderer;

	void enable(double sampleRate, ARA::ARAChannelCount channels, ARA::ARASampleCount maxFrames) {
		_sampleRate = sampleRate;
		_channels = std::min(channels, kMaxChannels);
		_scratch.assign(static_cast<size_t>(_channels) * static_cast<size_t>(std::max<ARA::ARASampleCount>(maxFrames, 1)), 0.0f);
		_maxFrames = maxFrames;
	}

	void render(float* const* output, ARA::ARAChannelCount channels, ARA::ARASamplePosition position, ARA::ARASampleCount frames,
	            bool playing, RegionCallback callback, void* ctx);

private:
	double _sampleRate = 44100.0;
	ARA::ARAChannelCount _channels = 2;
	ARA::ARASampleCount _maxFrames = 0;
	std::vector<float> _scratch;
};

class NotesReader final : public ARA::PlugIn::ContentReader {
public:
	NotesReader(const std::vector<NoteData>& notes, const ARA::ARAContentTimeRange* range) {
		for (const auto& n : notes) {
			if (range && !(n.start >= range->start && n.start < range->start + range->duration))
				continue;
			ARA::ARAContentNote note{};
			note.frequency = n.pitch == INT32_MIN ? ARA::kARAInvalidFrequency : n.frequency;
			note.pitchNumber = n.pitch == INT32_MIN ? ARA::kARAInvalidPitchNumber : static_cast<ARA::ARAPitchNumber>(n.pitch);
			note.volume = n.volume;
			note.startPosition = n.start;
			note.attackDuration = n.attack;
			note.noteDuration = n.duration;
			note.signalDuration = n.signal;
			_notes.push_back(note);
		}
	}
	ARA::ARAInt32 getEventCount() noexcept override { return static_cast<ARA::ARAInt32>(_notes.size()); }
	const void* getDataForEvent(ARA::ARAInt32 index) noexcept override { return &_notes[static_cast<size_t>(index)]; }

private:
	std::vector<ARA::ARAContentNote> _notes;
};

class DocumentController final : public ARA::PlugIn::DocumentController {
public:
	using ARA::PlugIn::DocumentController::DocumentController;

	// Renderers stay out of the model while the host edits it.
	bool rendererWillAccessModel() noexcept {
		if (!_renderersMayAccess.load(std::memory_order_acquire))
			return false;
		++_renderersAccessing;
		if (!_renderersMayAccess.load(std::memory_order_acquire)) {
			--_renderersAccessing;
			return false;
		}
		return true;
	}
	void rendererDidAccessModel() noexcept { --_renderersAccessing; }

protected:
	void willBeginEditing() noexcept override {
		_renderersMayAccess.store(false, std::memory_order_release);
		while (_renderersAccessing.load(std::memory_order_acquire) > 0)
			std::this_thread::yield();
	}
	void didEndEditing() noexcept override { _renderersMayAccess.store(true, std::memory_order_release); }

	// Ferritron keeps no per-object data in the document archive yet; the
	// plugin's parameters travel with the format's own state.
	bool doRestoreObjectsFromArchive(ARA::PlugIn::HostArchiveReader*, const ARA::PlugIn::RestoreObjectsFilter*) noexcept override {
		return true;
	}
	bool doStoreObjectsToArchive(ARA::PlugIn::HostArchiveWriter*, const ARA::PlugIn::StoreObjectsFilter*) noexcept override {
		return true;
	}

	// Tempo and signature are not used by the default rendering.
	void doUpdateMusicalContextContent(ARA::PlugIn::MusicalContext*, const ARA::ARAContentTimeRange*, ARA::ContentUpdateScopes) noexcept override {}

	ARA::PlugIn::AudioSource* doCreateAudioSource(ARA::PlugIn::Document* document, ARA::ARAAudioSourceHostRef hostRef) noexcept override {
		return new AudioSource(document, hostRef);
	}

	void doUpdateAudioSourceContent(ARA::PlugIn::AudioSource* source, const ARA::ARAContentTimeRange*, ARA::ContentUpdateScopes scopes) noexcept override {
		auto* s = static_cast<AudioSource*>(source);
		if (scopes.affectSamples() && s->isSampleAccessEnabled()) {
			s->updateCache();
			if (s->notesWanted())
				startNotes(s);
		}
	}

	void willEnableAudioSourceSamplesAccess(ARA::PlugIn::AudioSource*, bool) noexcept override {}

	void didEnableAudioSourceSamplesAccess(ARA::PlugIn::AudioSource* source, bool enable) noexcept override {
		auto* s = static_cast<AudioSource*>(source);
		if (enable && !s->cached())
			s->updateCache();
		if (enable && s->notesWanted())
			startNotes(s);
	}

	// --- content analysis: the notes of an audio source ---

	bool doIsAudioSourceContentAvailable(const ARA::PlugIn::AudioSource* source, ARA::ARAContentType type) noexcept override {
		return type == ARA::kARAContentTypeNotes && g_config.analyze && static_cast<const AudioSource*>(source)->notesState() >= 2;
	}

	ARA::ARAContentGrade doGetAudioSourceContentGrade(const ARA::PlugIn::AudioSource*, ARA::ARAContentType) noexcept override {
		return ARA::kARAContentGradeDetected;
	}

	ARA::PlugIn::ContentReader* doCreateAudioSourceContentReader(ARA::PlugIn::AudioSource* source, ARA::ARAContentType,
	                                                             const ARA::ARAContentTimeRange* range) noexcept override {
		return new NotesReader(static_cast<AudioSource*>(source)->notes(), range);
	}

	bool doIsAudioSourceContentAnalysisIncomplete(const ARA::PlugIn::AudioSource* source, ARA::ARAContentType type) noexcept override {
		if (type != ARA::kARAContentTypeNotes)
			return false;
		auto* s = const_cast<AudioSource*>(static_cast<const AudioSource*>(source));
		if (s->notesState() == 2) {
			// Finished on the analysis thread; the host hears about it from the main thread.
			s->notesTold();
			notifyAudioSourceContentChanged(s, ARA::ContentUpdateScopes::notesAreAffected());
		}
		return s->notesWanted() && s->notesState() < 2;
	}

	void doRequestAudioSourceContentAnalysis(ARA::PlugIn::AudioSource* source, std::vector<ARA::ARAContentType> const& types) noexcept override {
		if (std::find(types.begin(), types.end(), ARA::kARAContentTypeNotes) == types.end())
			return;
		auto* s = static_cast<AudioSource*>(source);
		s->wantNotes();
		if (s->isSampleAccessEnabled() && s->cached())
			startNotes(s);
	}

	void willUpdateAudioSourceProperties(ARA::PlugIn::AudioSource* source, ARA::PlugIn::PropertiesPtr<ARA::ARAAudioSourceProperties> properties) noexcept override {
		// A new length or channel count makes the cached samples stale.
		if (properties->sampleCount != source->getSampleCount() || properties->channelCount != source->getChannelCount())
			static_cast<AudioSource*>(source)->clearCache();
	}

	ARA::PlugIn::PlaybackRenderer* doCreatePlaybackRenderer() noexcept override { return new PlaybackRenderer(this); }

private:
	void startNotes(AudioSource* s) {
		if (s->notesState() != 0)
			return;
		notifyAudioSourceAnalysisProgressStarted(s);
		s->startAnalysis([this, s] { notifyAudioSourceAnalysisProgressCompleted(s); });
	}

	std::atomic<bool> _renderersMayAccess{true};
	std::atomic<int> _renderersAccessing{0};
};

void PlaybackRenderer::render(float* const* output, ARA::ARAChannelCount channels, ARA::ARASamplePosition position,
                              ARA::ARASampleCount frames, bool playing, RegionCallback callback, void* ctx) {
	for (ARA::ARAChannelCount c = 0; c < channels; ++c)
		std::memset(output[c], 0, sizeof(float) * static_cast<size_t>(frames));
	if (!playing || frames <= 0)
		return;
	channels = std::min(channels, kMaxChannels);
	auto* controller = getDocumentController<DocumentController>();
	if (!controller->rendererWillAccessModel())
		return;
	const auto end = position + frames;
	for (const auto& region : getPlaybackRegions()) {
		const auto* source = region->getAudioModification()->getAudioSource<const AudioSource>();
		if (!source->isSampleAccessEnabled() || !source->cached() || source->getSampleRate() != _sampleRate)
			continue;
		// The part of the region inside this block, in song samples.
		const auto regionStart = region->getStartInPlaybackSamples(_sampleRate);
		const auto regionEnd = region->getEndInPlaybackSamples(_sampleRate);
		auto from = std::max(regionStart, position);
		auto to = std::min(regionEnd, end);
		// Source samples per playback sample: 1 unless the host stretched the region.
		double ratio = 0.0;
		const auto playbackTime = region->getDurationInPlaybackTime();
		const auto modificationTime = region->getDurationInAudioModificationTime();
		if (g_config.stretch && region->isTimestretchEnabled() && playbackTime > 0.0 && modificationTime > 0.0 &&
		    std::fabs(modificationTime / playbackTime - 1.0) > 1e-6)
			ratio = modificationTime / playbackTime;
		double sourceStart;
		if (ratio == 0.0) {
			const auto offset = region->getStartInAudioModificationSamples() - regionStart; // song -> source
			from = std::max(from, std::max<ARA::ARASamplePosition>(0, region->getStartInAudioModificationSamples()) - offset);
			to = std::min(to, std::min<ARA::ARASamplePosition>(static_cast<ARA::ARASamplePosition>(source->cachedSamples()),
			                                                  region->getEndInAudioModificationSamples()) - offset);
			sourceStart = static_cast<double>(from + offset);
		} else {
			sourceStart = static_cast<double>(region->getStartInAudioModificationSamples()) + static_cast<double>(from - regionStart) * ratio;
		}
		if (to <= from)
			continue;
		const auto count = static_cast<size_t>(to - from);
		if (count * static_cast<size_t>(channels) > _scratch.size())
			continue; // larger than the announced maximum block
		const float* sourceChannels[kMaxChannels] = {};
		const auto sourceCount = std::min<size_t>(source->cachedChannels(), kMaxChannels);
		for (size_t c = 0; c < sourceCount; ++c)
			sourceChannels[c] = source->channel(c);
		float* scratch[kMaxChannels] = {};
		for (ARA::ARAChannelCount c = 0; c < channels; ++c) {
			scratch[c] = _scratch.data() + static_cast<size_t>(c) * count;
			std::memset(scratch[c], 0, sizeof(float) * count);
		}
		callback(ctx, sourceChannels, static_cast<uint32_t>(sourceCount), static_cast<int64_t>(source->cachedSamples()),
		         source->getSampleRate(), sourceStart, ratio, static_cast<int64_t>(from - regionStart), scratch,
		         static_cast<uint32_t>(channels), static_cast<uint32_t>(count));
		// Overlapping regions add up.
		const auto at = static_cast<size_t>(from - position);
		for (ARA::ARAChannelCount c = 0; c < channels; ++c)
			for (size_t i = 0; i < count; ++i)
				output[c][at + i] += scratch[c][i];
	}
	controller->rendererDidAccessModel();
}

class FactoryConfig : public ARA::PlugIn::FactoryConfig {
public:
	ARA::ARAAPIGeneration getHighestSupportedApiGeneration() const noexcept override { return ARA::kARAAPIGeneration_2_3_Final; }
	const char* getFactoryID() const noexcept override { return g_config.factoryId.c_str(); }
	const char* getPlugInName() const noexcept override { return g_config.name.c_str(); }
	const char* getManufacturerName() const noexcept override { return g_config.vendor.c_str(); }
	const char* getInformationURL() const noexcept override { return g_config.url.c_str(); }
	const char* getVersion() const noexcept override { return g_config.version.c_str(); }
	const char* getDocumentArchiveID() const noexcept override { return g_config.archiveId.c_str(); }
	ARA::ARASize getAnalyzeableContentTypesCount() const noexcept override { return g_config.analyze ? 1 : 0; }
	const ARA::ARAContentType* getAnalyzeableContentTypes() const noexcept override {
		if (!g_config.analyze)
			return nullptr;
		static const ARA::ARAContentType types[] = {ARA::kARAContentTypeNotes};
		return types;
	}
	ARA::ARAPlaybackTransformationFlags getSupportedPlaybackTransformationFlags() const noexcept override {
		return g_config.stretch ? ARA::kARAPlaybackTransformationTimestretch : ARA::kARAPlaybackTransformationNoChanges;
	}
};

} // namespace

extern "C" {

struct ferritron_ara_config {
	const char* factory_id;
	const char* name;
	const char* vendor;
	const char* url;
	const char* version;
	const char* archive_id;
	bool stretch;
	AnalyzeCallback analyze;
};

// The plugin's ARA factory; the first call sets its description.
const ARA::ARAFactory* ferritron_ara_factory(const ferritron_ara_config* config) {
	static const bool configured = [config] {
		g_config = {config->factory_id, config->name, config->vendor, config->url, config->version, config->archive_id, config->stretch, config->analyze};
		return true;
	}();
	(void)configured;
	return ARA::PlugIn::PlugInEntry::getPlugInEntry<FactoryConfig, DocumentController>()->getFactory();
}

// One per plugin instance, created when the host first binds it.
void* ferritron_ara_new_extension() { return new (std::nothrow) ARA::PlugIn::PlugInExtension(); }

void ferritron_ara_delete_extension(void* extension) { delete static_cast<ARA::PlugIn::PlugInExtension*>(extension); }

const ARA::ARAPlugInExtensionInstance* ferritron_ara_bind(void* extension, ARA::ARADocumentControllerRef controller,
                                                          ARA::ARAPlugInInstanceRoleFlags known, ARA::ARAPlugInInstanceRoleFlags assigned) {
	return static_cast<ARA::PlugIn::PlugInExtension*>(extension)->bindToARA(controller, known, assigned);
}

void ferritron_ara_set_rendering(void* extension, bool enable, double sampleRate, uint32_t channels, uint32_t maxFrames) {
	auto* renderer = static_cast<ARA::PlugIn::PlugInExtension*>(extension)->getPlaybackRenderer<PlaybackRenderer>();
	if (renderer && enable)
		renderer->enable(sampleRate, static_cast<ARA::ARAChannelCount>(channels), static_cast<ARA::ARASampleCount>(maxFrames));
}

// Whether the instance is a playback renderer; if so, `channels` now hold its regions.
bool ferritron_ara_render(void* extension, float* const* channels, uint32_t numChannels, uint32_t frames, int64_t position,
                          bool playing, RegionCallback callback, void* ctx) {
	auto* renderer = static_cast<ARA::PlugIn::PlugInExtension*>(extension)->getPlaybackRenderer<PlaybackRenderer>();
	if (!renderer)
		return false;
	renderer->render(channels, static_cast<ARA::ARAChannelCount>(numChannels), position, frames, playing, callback, ctx);
	return true;
}

} // extern "C"
