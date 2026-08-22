using Mw.Native;
using UnityEngine;
using UnityEngine.UI;

namespace Measurement
{
    /// <summary>
    /// M1 A/B 計測用シーンのコントローラ(docs/measurement-m1.md)。
    /// <list type="bullet">
    /// <item>ボタンA: Unity 既定実装(<see cref="AudioSource.PlayOneShot(AudioClip)"/>)</item>
    /// <item>ボタンB: 本ミドルウェア(<see cref="MwNative.PlaySe"/>)</item>
    /// </list>
    /// 両方式とも <see cref="ClickSeGenerator"/> が生成する同一波形のクリック SE を鳴らし、
    /// タップと同一フレームで <see cref="ScreenFlash"/> により画面を白フラッシュする
    /// (外部録音でのトリガー時刻特定用、init.md 要件)。
    /// フィールドはシーン組み立てスクリプト(<c>Measurement.EditorTools.MeasurementSceneBuilder</c>)
    /// から配線される想定のため public にしてある。
    /// </summary>
    public class MeasurementController : MonoBehaviour
    {
        public ScreenFlash screenFlash;
        public Text statusText;
        public AudioSource unityAudioSource;
        public Button buttonA;
        public Button buttonB;

        private ulong _handle;
        private ulong _soundId;
        private bool _nativeReady;

        /// <summary>
        /// ネイティブ側の状態を画面へ常時表示するための文字列。
        /// 初期化に失敗しても <see cref="Debug.LogError"/> だけでは実機・録画から気づけず、
        /// 「B が1打も鳴っていない」ことに計測後まで気づけなかったため
        /// (docs/measurement-m1.md §7.6-1)、画面に出して録画に残す。
        /// </summary>
        private string _nativeStatus = "native: (not initialized)";

        private void Awake()
        {
            if (buttonA != null)
            {
                buttonA.onClick.AddListener(TriggerA);
            }

            if (buttonB != null)
            {
                buttonB.onClick.AddListener(TriggerB);
            }

            if (unityAudioSource != null)
            {
                unityAudioSource.playOnAwake = false;
                unityAudioSource.clip = ClickSeGenerator.BuildAudioClip();
            }

            InitializeNative();
            SetStatus("Mode: - (未タップ / not tapped yet)");
        }

        private void OnDestroy()
        {
            // mw_sound_release / mw_shutdown は冪等ではないため、成功した初期化状態のときだけ呼ぶ
            // (crates/mw-ffi/CLAUDE.md, unity/Runtime/MwNative.cs)。
            if (_nativeReady)
            {
                MwNative.ReleaseSound(_handle, _soundId);
                MwNative.Shutdown(_handle);
            }
        }

        private void InitializeNative()
        {
            MwResult initResult = MwNative.Init(out _handle);
            if (initResult != MwResult.Ok)
            {
                Debug.LogError($"MeasurementController: mw_init failed ({initResult})");
                _nativeStatus = $"native: INIT FAILED ({initResult})";
                return;
            }

            byte[] wavBytes = ClickSeGenerator.BuildWavBytes();
            MwResult loadResult = MwNative.LoadSound(_handle, wavBytes, SoundMode.Se, out _soundId);
            if (loadResult != MwResult.Ok)
            {
                Debug.LogError($"MeasurementController: mw_sound_load failed ({loadResult})");
                _nativeStatus = $"native: LOAD FAILED ({loadResult})";
                MwNative.Shutdown(_handle);
                return;
            }

            _nativeReady = true;
            _nativeStatus = "native: ready";
        }

        /// <summary>ボタンA: Unity 既定実装で発音し、同一フレームで白フラッシュする。</summary>
        public void TriggerA()
        {
            if (screenFlash != null)
            {
                screenFlash.Flash();
            }

            if (unityAudioSource != null && unityAudioSource.clip != null)
            {
                unityAudioSource.PlayOneShot(unityAudioSource.clip);
            }

            SetStatus("Mode: A (Unity AudioSource.PlayOneShot)");
        }

        /// <summary>ボタンB: ネイティブミドルウェアで発音し、同一フレームで白フラッシュする。</summary>
        public void TriggerB()
        {
            if (screenFlash != null)
            {
                screenFlash.Flash();
            }

            if (_nativeReady)
            {
                MwResult playResult = MwNative.PlaySe(_handle, _soundId, Bus.Se, volume: 1f, voice: out _);
                if (playResult != MwResult.Ok)
                {
                    // 戻り値を捨てていると「鳴らないのに理由が分からない」状態になる
                    // (docs/measurement-m1.md §7.6-1)。
                    Debug.LogError($"MeasurementController: mw_se_play failed ({playResult})");
                    _nativeStatus = $"native: PLAY FAILED ({playResult})";
                }
            }
            else
            {
                Debug.LogWarning("MeasurementController: native middleware is not ready; B is silent this tap.");
            }

            SetStatus("Mode: B (Mw.Native.MwNative.PlaySe)");
        }

        private void SetStatus(string text)
        {
            if (statusText != null)
            {
                statusText.text = $"{text}    |    {_nativeStatus}";
            }
        }
    }
}
